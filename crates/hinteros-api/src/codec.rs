//! NDJSON framing: one UTF-8 JSON value per line, terminated by `\n`.

use std::io;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use zeroize::Zeroizing;

/// Upper bound for one message, excluding the newline (matches `max_message_bytes` in the schema).
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("message exceeds the size limit")]
    TooLong,
    #[error("message is not valid UTF-8")]
    NotUtf8,
}

pub struct LineReader<R> {
    inner: BufReader<R>,
    buf: Vec<u8>,
    max: usize,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_MESSAGE_BYTES)
    }

    /// A reader whose lines may be at most `max` bytes (the system socket uses 64 KiB).
    pub fn with_limit(inner: R, max: usize) -> Self {
        Self {
            inner: BufReader::new(inner),
            buf: Vec::new(),
            max,
        }
    }

    /// Discard whatever the peer still sends, until EOF, `max_bytes` or `within` runs out.
    ///
    /// Closing a socket with unread input makes the kernel send RST, which can destroy the
    /// reply we just wrote before the peer has read it. Call this after `shutdown`ing the
    /// write side so the peer sees the reply and a clean EOF.
    pub async fn drain(&mut self, max_bytes: usize, within: std::time::Duration) {
        self.buf.clear();
        let _ = tokio::time::timeout(within, async {
            let mut left = max_bytes;
            let mut sink = [0u8; 8192];
            while left > 0 {
                match self.inner.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => left = left.saturating_sub(n),
                }
            }
        })
        .await;
    }

    /// Next line without its terminator; `None` at end of stream. Blank lines are skipped.
    ///
    /// Cancel safe: partially read data stays buffered for the next call, so this can be
    /// used as a `tokio::select!` branch.
    pub async fn next_line(&mut self) -> Result<Option<String>, ReadError> {
        loop {
            let limit = (self.max + 2).saturating_sub(self.buf.len()) as u64;
            let n = (&mut self.inner)
                .take(limit)
                .read_until(b'\n', &mut self.buf)
                .await?;
            let complete = self.buf.last() == Some(&b'\n');
            if !complete && n != 0 {
                if self.buf.len() > self.max {
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
            if line.len() > self.max {
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

/// The length of `message` serialized as compact JSON, without making a copy of it.
pub fn serialized_len(message: &impl Serialize) -> serde_json::Result<usize> {
    struct Count(usize);
    impl io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, message)?;
    Ok(count.0)
}

/// Serialize a message that carries a secret (a PIN) into one buffer of exactly its size (a
/// growing buffer would leave copies behind), wiped when it is dropped. With the newline.
pub fn secret_line(message: &impl Serialize) -> serde_json::Result<Zeroizing<Vec<u8>>> {
    let len = serialized_len(message)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(len + 1));
    serde_json::to_writer(&mut *bytes, message)?;
    bytes.push(b'\n');
    debug_assert_eq!(bytes.capacity(), len + 1, "no reallocation");
    Ok(bytes)
}

/// [`write_message`] for a message that carries a secret: see [`secret_line`].
pub async fn write_secret_message<W: AsyncWrite + Unpin>(
    w: &mut W,
    message: &impl Serialize,
) -> io::Result<()> {
    let bytes = secret_line(message)?;
    w.write_all(&bytes).await?;
    w.flush().await
}

/// How much input a closing connection discards, and for how long, before the real close.
pub const CLOSE_DRAIN_BYTES: usize = 4 << 20;
pub const CLOSE_DRAIN_TIME: std::time::Duration = std::time::Duration::from_secs(2);

/// Close a connection the server ends on purpose without resetting it: half-close our side
/// (the peer reads everything we sent, then EOF), then discard the peer's pending input
/// briefly so the final close carries no unread data (which the kernel turns into an RST).
pub async fn close_gracefully<R, W>(reader: &mut LineReader<R>, w: &mut W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let _ = w.shutdown().await;
    reader.drain(CLOSE_DRAIN_BYTES, CLOSE_DRAIN_TIME).await;
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

    #[test]
    fn secret_lines_are_ordinary_lines() {
        let m = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "m", "params": {"pin": "1357", "name": "Ünï"}});
        let line = secret_line(&m).unwrap();
        let mut want = serde_json::to_vec(&m).unwrap();
        want.push(b'\n');
        assert_eq!(&line[..], &want[..]);
        assert_eq!(line.capacity(), line.len(), "one exact allocation");
        assert_eq!(serialized_len(&m).unwrap(), want.len() - 1);
    }
}
