//! A small async client, used by the session daemon to reach the system service and by tests.

use std::collections::VecDeque;
use std::io;
use std::path::Path;

use serde_json::Value;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::codec::{LineReader, ReadError, write_message};
use crate::common::{Hello, HelloParams, HelloResult, Peer};
use crate::jsonrpc::{self, Id, ServerMessage};
use crate::{ApiVersion, ErrorObject, Event, Method};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error("server error: {0}")]
    Rpc(#[from] ErrorObject),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("connection closed by server")]
    Closed,
}

pub struct Client {
    reader: LineReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: i64,
    events: VecDeque<(String, Value)>,
}

impl Client {
    /// Connect without handshaking (see [`Client::hello`]).
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let (r, w) = UnixStream::connect(path).await?.into_split();
        Ok(Self {
            reader: LineReader::new(r),
            writer: w,
            next_id: 1,
            events: VecDeque::new(),
        })
    }

    pub async fn hello(
        &mut self,
        api_version: ApiVersion,
        client: Peer,
    ) -> Result<HelloResult, ClientError> {
        self.call::<Hello>(&HelloParams {
            api_version,
            client,
        })
        .await
    }

    pub async fn call<M: Method>(&mut self, params: &M::Params) -> Result<M::Result, ClientError> {
        let params = serde_json::to_value(params).map_err(io::Error::from)?;
        let result = self.call_raw(M::NAME, params).await?;
        serde_json::from_value(result).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// Call a method; `Err(ClientError::Rpc(_))` carries the server's error object.
    pub async fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = Id::Number(self.next_id);
        self.next_id += 1;
        write_message(&mut self.writer, &jsonrpc::request(&id, method, params)).await?;
        loop {
            match self.read_message().await? {
                ServerMessage::Notification { method, params } => {
                    self.events.push_back((method, params))
                }
                ServerMessage::Response {
                    id: Some(got),
                    outcome,
                } if got == id => return outcome.map_err(ClientError::Rpc),
                other => {
                    return Err(ClientError::Protocol(format!(
                        "unexpected message: {other:?}"
                    )));
                }
            }
        }
    }

    /// Next event (server notification) as raw name and params.
    pub async fn next_event_raw(&mut self) -> Result<(String, Value), ClientError> {
        if let Some(ev) = self.events.pop_front() {
            return Ok(ev);
        }
        match self.read_message().await? {
            ServerMessage::Notification { method, params } => Ok((method, params)),
            other => Err(ClientError::Protocol(format!(
                "unexpected message: {other:?}"
            ))),
        }
    }

    /// Wait for the next event, which must be an `E`.
    pub async fn next_event<E: Event>(&mut self) -> Result<E::Params, ClientError> {
        let (name, params) = self.next_event_raw().await?;
        if name != E::NAME {
            return Err(ClientError::Protocol(format!(
                "expected event {}, got {name}",
                E::NAME
            )));
        }
        serde_json::from_value(params).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// Send a raw line (for tests of malformed input).
    pub async fn send_line(&mut self, line: &str) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await
    }

    /// Read one raw message; `None` once the server has closed the connection.
    pub async fn read_raw(&mut self) -> Result<Option<Value>, ClientError> {
        match self.reader.next_line().await? {
            None => Ok(None),
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| ClientError::Protocol(e.to_string())),
        }
    }

    async fn read_message(&mut self) -> Result<ServerMessage, ClientError> {
        let value = self.read_raw().await?.ok_or(ClientError::Closed)?;
        jsonrpc::parse_server_message(value).map_err(ClientError::Protocol)
    }
}
