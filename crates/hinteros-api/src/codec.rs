//! NDJSON framing: one UTF-8 JSON value per line, terminated by `\n`.

use std::io;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Upper bound for one message, excluding the newline (matches `max_message_bytes` in the schema).
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("message exceeds {MAX_MESSAGE_BYTES} bytes")]
    TooLong,
    #[error("message is not valid UTF-8")]
    NotUtf8,
}

pub struct LineReader<R> {
    inner: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            buf: Vec::new(),
        }
    }

    /// Next line without its terminator; `None` at end of stream. Blank lines are skipped.
    ///
    /// Cancel safe: partially read data stays buffered for the next call, so this can be
    /// used as a `tokio::select!` branch.
    pub async fn next_line(&mut self) -> Result<Option<String>, ReadError> {
        loop {
            let limit = (MAX_MESSAGE_BYTES + 2).saturating_sub(self.buf.len()) as u64;
            let n = (&mut self.inner)
                .take(limit)
                .read_until(b'\n', &mut self.buf)
                .await?;
            let complete = self.buf.last() == Some(&b'\n');
            if !complete && n != 0 {
                if self.buf.len() > MAX_MESSAGE_BYTES {
                    return Err(ReadError::TooLong);
                }
                continue;
            }
            if n == 0 && self.buf.is_empty() {
                return Ok(None);
            }
            let mut line = std::mem::take(&mut self.buf);
            if complete {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.len() > MAX_MESSAGE_BYTES {
                return Err(ReadError::TooLong);
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                if n == 0 {
                    return Ok(None);
                }
                continue;
            }
            return String::from_utf8(line)
                .map(Some)
                .map_err(|_| ReadError::NotUtf8);
        }
    }
}

/// Write one message as a single line and flush.
pub async fn write_message<W: AsyncWrite + Unpin>(
    w: &mut W,
    message: &impl Serialize,
) -> io::Result<()> {
    // serde_json's compact output never contains a raw newline.
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn splits_lines() {
        let data: &[u8] = b"{\"a\":1}\n\n{\"b\":2}\r\n{\"c\":3}";
        let mut r = LineReader::new(data);
        assert_eq!(r.next_line().await.unwrap().unwrap(), "{\"a\":1}");
        assert_eq!(r.next_line().await.unwrap().unwrap(), "{\"b\":2}");
        assert_eq!(r.next_line().await.unwrap().unwrap(), "{\"c\":3}");
        assert!(r.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_long_lines() {
        let data = vec![b'x'; MAX_MESSAGE_BYTES + 10];
        let mut r = LineReader::new(&data[..]);
        assert!(matches!(r.next_line().await, Err(ReadError::TooLong)));

        let mut ok = vec![b'x'; MAX_MESSAGE_BYTES];
        ok.push(b'\n');
        let mut r = LineReader::new(&ok[..]);
        assert_eq!(
            r.next_line().await.unwrap().unwrap().len(),
            MAX_MESSAGE_BYTES
        );
    }
}
