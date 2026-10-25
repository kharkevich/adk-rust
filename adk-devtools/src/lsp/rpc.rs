//! JSON-RPC 2.0 over the LSP base protocol: `Content-Length` framed messages on a
//! server's standard streams.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::oneshot;

/// The largest message body read from a server; a longer one ends the connection.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// The longest header line read from a server.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// An error a server answered a request with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

/// LSP `ContentModified`: the document changed while the request ran.
pub(crate) const CONTENT_MODIFIED: i64 = -32801;
/// LSP `ServerCancelled`: the server cancelled the request and asks for a retry.
pub(crate) const SERVER_CANCELLED: i64 = -32802;
/// JSON-RPC `MethodNotFound`.
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;

/// Why a request got no result.
#[derive(Debug)]
pub(crate) enum CallError {
    /// The server answered with an error.
    Rpc(RpcError),
    /// No answer within the timeout; the request was cancelled.
    Timeout,
    /// The connection is gone.
    Closed(String),
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

/// Reads one framed message; `None` at the end of the stream.
pub(crate) async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut *reader).take(MAX_HEADER_BYTES as u64).read_until(b'\n', &mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        if !line.ends_with(b"\n") {
            return Err(invalid("a header line is longer than 8 KiB or ends the stream"));
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            match length {
                Some(_) => break,
                // Blank lines before a header carry nothing.
                None => continue,
            }
        }
        if let Some((name, value)) = text.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            let parsed: usize =
                value.trim().parse().map_err(|_| invalid("Content-Length is not a number"))?;
            if parsed > MAX_MESSAGE_BYTES {
                return Err(invalid(format!("a {parsed}-byte message is over the 64 MiB cap")));
            }
            length = Some(parsed);
        }
    }
    let length = length.unwrap_or_default();
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map(Some).map_err(|error| invalid(error.to_string()))
}

/// `message` with its `Content-Length` header.
pub(crate) fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// The client side of one connection: requests with ids, their answers, and
/// notifications. The read side runs in the server's reader task, which hands answers to
/// [`complete`](Self::complete).
pub(crate) struct Connection {
    writer: tokio::sync::Mutex<Writer>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>,
    next_id: AtomicI64,
    closed: Mutex<Option<String>>,
}

impl Connection {
    pub(crate) fn new(writer: Writer) -> Self {
        Self {
            writer: tokio::sync::Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            closed: Mutex::new(None),
        }
    }

    async fn send(&self, message: &Value) -> Result<(), CallError> {
        if let Some(reason) = self.closed.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Err(CallError::Closed(reason));
        }
        let bytes = frame(message);
        let mut writer = self.writer.lock().await;
        let written = async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        };
        written.await.map_err(|error| CallError::Closed(format!("writing to the server failed: {error}")))
    }

    /// Sends a notification.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<(), CallError> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// Answers a request the server made.
    pub(crate) async fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let message = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": error.code, "message": error.message },
            }),
        };
        if let Err(CallError::Closed(reason)) = self.send(&message).await {
            tracing::debug!(reason = %reason, "language server request left unanswered");
        }
    }

    /// Sends a request and waits for its answer, at most `timeout`; on a timeout the
    /// request is cancelled with `$/cancelRequest`.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(error) = self.send(&message).await {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(CallError::Rpc(error)),
            Ok(Err(_)) => Err(CallError::Closed(
                self.closed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_else(|| "the connection closed".to_string()),
            )),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
                Err(CallError::Timeout)
            }
        }
    }

    /// Hands the answer to request `id` to its waiter.
    pub(crate) fn complete(&self, id: i64, result: Result<Value, RpcError>) {
        if let Some(tx) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// Fails every waiting request and every later one with `reason`.
    pub(crate) fn close(&self, reason: &str) {
        self.closed.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| reason.to_string());
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_tolerate_extra_headers() {
        let message = json!({ "jsonrpc": "2.0", "method": "x", "params": { "é": 1 } });
        let mut bytes = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n".to_vec();
        bytes.extend(frame(&message));
        bytes.extend(frame(&json!({ "jsonrpc": "2.0", "id": 1, "result": null })));
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert_eq!(read_message(&mut reader).await.unwrap(), Some(message));
        assert_eq!(
            read_message(&mut reader).await.unwrap(),
            Some(json!({ "jsonrpc": "2.0", "id": 1, "result": null }))
        );
        assert_eq!(read_message(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_oversized_or_garbled_message_is_an_error() {
        let bytes = b"Content-Length: 999999999999\r\n\r\n".to_vec();
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert!(read_message(&mut reader).await.is_err());
        let bytes = b"Content-Length: 3\r\n\r\nabc".to_vec();
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert!(read_message(&mut reader).await.is_err());
        let bytes = vec![b'x'; MAX_HEADER_BYTES + 10];
        let mut reader = tokio::io::BufReader::new(&bytes[..]);
        assert!(read_message(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn a_request_times_out_and_is_cancelled() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let connection = Connection::new(Box::new(client));
        let error = connection.request("slow", json!({}), Duration::from_millis(50)).await;
        assert!(matches!(error, Err(CallError::Timeout)), "{error:?}");
        let mut reader = tokio::io::BufReader::new(server);
        let sent = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(sent["method"], "slow");
        let cancel = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(cancel, json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": sent["id"] } }));
    }
}
