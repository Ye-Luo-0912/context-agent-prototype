//! The one bounded frame codec shared by every JSON-lines process boundary
//! (the `ProcessHost`, the context-service binary and the MCP stdio client).
//!
//! Both directions are capped symmetrically:
//!
//! - **Outbound** (`encode_frame`): a request/answer is serialized and
//!   rejected *before* a single byte reaches the pipe, so an over-cap frame
//!   can never leave a half-written line the peer would mis-frame. The
//!   connection stays usable (nothing was written).
//! - **Inbound** (`read_frame`): a frame is read incrementally with an
//!   in-flight cap, so an over-cap line is rejected while reading instead of
//!   being buffered in full first. EOF and partial EOF (bytes without a
//!   terminating newline) are typed errors. Bytes after one newline stay
//!   buffered for the next frame: pipes and sockets are byte streams, so an
//!   OS read boundary cannot be treated as a protocol boundary.
//!
//! Callers decide the failure policy; ping-pong hosts
//! (`ProcessHost`, the context service) poison and terminate the session on
//! every framing error so a half-consumed exchange can never corrupt a
//! later request/response pair.

use std::fmt;

use agent_contracts::{AgentError, AgentResult};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// A typed framing failure from `read_frame`. Every kind is a session-level
/// violation: the caller must poison and terminate the owned connection
/// rather than guess at the remaining bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameErrorKind {
    /// The line exceeds the in-flight byte cap; rejected while reading.
    Oversize { limit: usize },
    /// The peer closed the stream before any byte of the frame.
    Eof,
    /// The peer closed the stream mid-frame (bytes, but no newline).
    PartialEof,
    /// The underlying read failed with an IO error.
    Io(String),
    /// 会话已毒化：半消耗的字节流不能再当下一帧读。
    Poisoned { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    pub kind: FrameErrorKind,
    /// Bytes accepted before the failure (for diagnostics).
    pub bytes: usize,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            FrameErrorKind::Oversize { limit } => write!(
                f,
                "frame exceeded the {limit} byte limit ({} bytes before rejection)",
                self.bytes
            ),
            FrameErrorKind::Eof => write!(f, "peer closed the connection before a frame"),
            FrameErrorKind::PartialEof => write!(
                f,
                "peer closed the connection mid-frame ({} bytes, no terminating newline)",
                self.bytes
            ),
            FrameErrorKind::Io(message) => write!(f, "frame read failed: {message}"),
            FrameErrorKind::Poisoned { reason } => write!(f, "session poisoned: {reason}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<FrameError> for AgentError {
    fn from(error: FrameError) -> Self {
        match &error.kind {
            FrameErrorKind::Io(message) => AgentError::Io(message.clone()),
            FrameErrorKind::Eof
            | FrameErrorKind::PartialEof
            | FrameErrorKind::Oversize { .. }
            | FrameErrorKind::Poisoned { .. } => AgentError::InvalidRequest(error.to_string()),
        }
    }
}

/// Serialize one frame with the outbound bound. Rejects an over-cap value
/// before anything is written, so the peer never sees a truncated line and
/// the caller's connection remains clean. The returned bytes include the
/// terminating newline.
pub fn encode_frame(value: &serde_json::Value, max_frame_bytes: usize) -> AgentResult<Vec<u8>> {
    let line = serde_json::to_string(value)
        .map_err(|e| AgentError::Context(format!("serialize frame: {e}")))?;
    encode_frame_bytes(line.as_bytes(), max_frame_bytes)
}

/// 把已经序列化的 JSON 正文封成一帧（补上终止换行）。
///
/// 内嵌换行或超长都会在写管道之前拒绝，连接仍然同步、可继续用。
/// 适配器（例如 operation-control）应走这条路径，避免把信封再经
/// `serde_json::Value` 转一圈。
pub fn encode_frame_bytes(payload: &[u8], max_frame_bytes: usize) -> AgentResult<Vec<u8>> {
    if payload.is_empty() {
        return Err(AgentError::InvalidRequest(
            "frame payload is empty; nothing was written".into(),
        ));
    }
    if payload.contains(&b'\n') {
        return Err(AgentError::InvalidRequest(
            "frame payload contains a newline; nothing was written".into(),
        ));
    }
    if payload.len() > max_frame_bytes {
        return Err(AgentError::InvalidRequest(format!(
            "frame is {} bytes, above the {max_frame_bytes} byte bound; nothing was written",
            payload.len()
        )));
    }
    let mut line = Vec::with_capacity(payload.len().saturating_add(1));
    line.extend_from_slice(payload);
    line.push(b'\n');
    Ok(line)
}

/// Read exactly one newline-terminated frame from an `AsyncBufRead`
/// (a `BufReader`, so bytes after the terminating newline stay buffered for
/// the next read).
///
/// - the in-flight cap is enforced before appending each chunk, so the
///   returned allocation never grows beyond `max_frame_bytes`;
/// - EOF before any byte is `Eof`; EOF mid-frame is `PartialEof` — never
///   silently accepted as a complete frame;
/// - bytes after the first newline remain in the reader. Session-level
///   request identities—not nondeterministic read chunking—detect stale or
///   pre-sent responses.
///
/// Returns the frame bytes without the trailing newline.
pub async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    max_frame_bytes: usize,
) -> Result<Vec<u8>, FrameError> {
    let mut frame: Vec<u8> = Vec::with_capacity(256.min(max_frame_bytes));
    loop {
        let buffer = match reader.fill_buf().await {
            Ok(buffer) => buffer,
            Err(e) => {
                return Err(FrameError {
                    kind: FrameErrorKind::Io(e.to_string()),
                    bytes: frame.len(),
                });
            }
        };
        if buffer.is_empty() {
            if frame.is_empty() {
                return Err(FrameError {
                    kind: FrameErrorKind::Eof,
                    bytes: 0,
                });
            }
            return Err(FrameError {
                kind: FrameErrorKind::PartialEof,
                bytes: frame.len(),
            });
        }
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                // The bound also applies when the newline arrives in the
                // same delivery as the whole over-cap line: a single large
                // fill_buf must not bypass the in-flight cap.
                let total = frame.len() + newline;
                if total > max_frame_bytes {
                    return Err(FrameError {
                        kind: FrameErrorKind::Oversize {
                            limit: max_frame_bytes,
                        },
                        bytes: total,
                    });
                }
                frame.extend_from_slice(&buffer[..newline]);
                let consumed = newline + 1;
                reader.consume(consumed);
                return Ok(frame);
            }
            None => {
                let remaining = max_frame_bytes.saturating_sub(frame.len());
                if buffer.len() > remaining {
                    return Err(FrameError {
                        kind: FrameErrorKind::Oversize {
                            limit: max_frame_bytes,
                        },
                        bytes: frame.len().saturating_add(buffer.len()),
                    });
                }
                frame.extend_from_slice(buffer);
                let consumed = buffer.len();
                reader.consume(consumed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncRead, BufReader, ReadBuf};

    #[test]
    fn encode_frame_rejects_oversize_before_writing() {
        let big = json!({ "payload": "x".repeat(1024) });
        let error = encode_frame(&big, 128).unwrap_err();
        assert!(error.to_string().contains("above the 128 byte bound"));
    }

    #[test]
    fn encode_frame_adds_the_newline() {
        let bytes = encode_frame(&json!({"op": "ping"}), 1024).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["op"], "ping");
    }

    #[test]
    fn encode_frame_bytes_rejects_embedded_newline_and_empty() {
        let newline = encode_frame_bytes(b"{\"a\":1}\n{\"b\":2}", 1024).unwrap_err();
        assert!(newline.to_string().contains("newline"));
        let empty = encode_frame_bytes(b"", 1024).unwrap_err();
        assert!(empty.to_string().contains("empty"));
    }

    /// A tiny reader that hands out at most 3 bytes per read, forcing the
    /// incremental frame reader to assemble across many small reads.
    struct Chunked<'a> {
        data: &'a [u8],
        offset: usize,
    }

    impl AsyncRead for Chunked<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let remaining = &self.data[self.offset..];
            let take = remaining.len().min(3).min(buf.remaining());
            buf.put_slice(&remaining[..take]);
            self.offset += take;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn read_frame_assembles_frames_across_chunks() {
        let source: &[u8] = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = BufReader::new(Chunked {
            data: source,
            offset: 0,
        });
        let first = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(first, b"{\"a\":1}");
        let second = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(second, b"{\"b\":2}");
        let eof = read_frame(&mut reader, 1024).await.unwrap_err();
        assert_eq!(eof.kind, FrameErrorKind::Eof);
    }

    #[tokio::test]
    async fn read_frame_rejects_oversize_while_reading() {
        let mut source: Vec<u8> = b"{\"payload\":\"".to_vec();
        source.extend(std::iter::repeat_n(b'x', 4096));
        source.extend_from_slice(b"\"}\n");
        let mut reader = BufReader::new(source.as_slice());
        let error = read_frame(&mut reader, 256).await.unwrap_err();
        assert!(
            matches!(error.kind, FrameErrorKind::Oversize { .. }),
            "oversize must be a typed error: {error:?}"
        );
        assert!(error.bytes >= 256, "the rejection happens while reading");
    }

    #[tokio::test]
    async fn oversize_chunk_is_rejected_before_the_result_vec_grows() {
        let source = vec![b'x'; 8192];
        let mut reader = BufReader::new(source.as_slice());
        let error = read_frame(&mut reader, 32).await.unwrap_err();
        assert!(matches!(error.kind, FrameErrorKind::Oversize { .. }));
        assert_eq!(
            reader.buffer().len(),
            8192,
            "the offending chunk stays unread"
        );
    }

    #[tokio::test]
    async fn read_frame_never_accepts_partial_eof() {
        let source: &[u8] = b"{\"a\":1"; // no newline, then EOF
        let mut reader = BufReader::new(source);
        let error = read_frame(&mut reader, 1024).await.unwrap_err();
        assert_eq!(error.kind, FrameErrorKind::PartialEof);
    }

    #[tokio::test]
    async fn read_chunking_never_defines_a_protocol_violation() {
        // Separate peer writes may be coalesced into one OS read. The codec
        // returns one frame and preserves the next; the session validates
        // request identities.
        let source: &[u8] = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = BufReader::new(source);
        let first = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(first, b"{\"a\":1}");
        let second = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(second, b"{\"b\":2}");
    }

    #[tokio::test]
    async fn stream_mode_preserves_the_remainder_for_the_next_read() {
        // Two frames in one delivery are normal for a stream protocol; the
        // second stays buffered for the next read.
        let source: &[u8] = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = BufReader::new(source);
        let first = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(first, b"{\"a\":1}");
        let second = read_frame(&mut reader, 1024).await.unwrap();
        assert_eq!(second, b"{\"b\":2}");
    }
}
