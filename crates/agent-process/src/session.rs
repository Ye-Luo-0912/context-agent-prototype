//! 有界 JSON-lines 会话：继承匿名管道的第一后端。
//!
//! 不拥有子进程、不谈授权。读用 [`read_frame`]，写用 [`encode_frame_bytes`]。
//! 入站帧错误会毒化会话，避免半消耗的字节流被当成下一帧。
//! 出站在写之前被拒绝时连接仍同步，与 [`encode_frame`] 一致。
//!
//! [`DuplexTransport`] is the byte-duplex seam; [`StdioDuplexTransport`] is
//! the inherited anonymous-pipe backend (PLAT-05). Named Pipe/UDS remain
//! PLAT-08 and must implement the same trait, not a second codec.
//!
//! 与 `AuthenticatedOperationControlAdapter::handle_frame` 的组合是：
//! 读一帧 → 把正文交给适配器 → 把返回的 JSON 正文 `send_bytes` 写回。
//! 本地传输身份本身不是 Core 授权。

use async_trait::async_trait;
use tokio::io::{
    AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
};

use agent_contracts::{AgentError, AgentResult};

use crate::frame::{FrameError, FrameErrorKind, encode_frame, encode_frame_bytes, read_frame};

/// Bounded framed byte duplex used by [`crate::ProcessHost`].
///
/// This is the PLAT-05 transport seam: protocol ping-pong talks to this
/// trait, not to `ChildStdout`. [`FramedProtocolSession`] is the
/// implementation; [`StdioDuplexTransport`] is the first backend.
/// Local transport identity is never a Core grant.
#[async_trait]
pub trait DuplexTransport: Send {
    fn is_poisoned(&self) -> bool;
    fn poison_reason(&self) -> Option<&str>;
    fn poison(&mut self, reason: String);
    async fn recv(&mut self) -> Result<Vec<u8>, FrameError>;
    /// Write a frame the protocol layer already capped (`encode_frame`).
    /// Encoding failure is not this method's job; a partial write poisons.
    async fn send_encoded_line(&mut self, line: &[u8]) -> AgentResult<()>;
}

/// 一条已经分好读写端的有界帧会话。单飞行：调用方必须 `recv` 后再 `send`。
pub struct FramedProtocolSession<R, W> {
    reader: R,
    writer: W,
    max_frame_bytes: usize,
    poisoned: Option<String>,
}

/// Inherited anonymous-pipe backend of [`DuplexTransport`].
/// Named Pipe/UDS remain PLAT-08.
pub type StdioDuplexTransport =
    FramedProtocolSession<BufReader<tokio::process::ChildStdout>, tokio::process::ChildStdin>;

impl<R, W> FramedProtocolSession<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    pub fn new(reader: R, writer: W, max_frame_bytes: usize) -> AgentResult<Self> {
        if max_frame_bytes == 0 {
            return Err(AgentError::InvalidRequest(
                "framed session byte bound must be positive".into(),
            ));
        }
        Ok(Self {
            reader,
            writer,
            max_frame_bytes,
            poisoned: None,
        })
    }

    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    pub fn poison_reason(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// 标记毒化。已经毒化时保留第一条原因。
    pub fn poison(&mut self, reason: impl Into<String>) {
        self.poisoned.get_or_insert_with(|| reason.into());
    }

    /// 读恰好一帧（不含换行）。干净 EOF 是 `Eof`，不毒化。
    /// 其它读错误会毒化，后续 `recv` 只返回 `Poisoned`。
    pub async fn recv(&mut self) -> Result<Vec<u8>, FrameError> {
        if let Some(reason) = &self.poisoned {
            return Err(FrameError {
                kind: FrameErrorKind::Poisoned {
                    reason: reason.clone(),
                },
                bytes: 0,
            });
        }
        match read_frame(&mut self.reader, self.max_frame_bytes).await {
            Ok(frame) => Ok(frame),
            Err(error) => {
                if !matches!(error.kind, FrameErrorKind::Eof) {
                    self.poison(error.to_string());
                }
                Err(error)
            }
        }
    }

    /// 写出已经序列化的 JSON 正文。编码失败不写字节、不毒化。
    /// 写到一半失败则毒化：对端可能已经看到残帧。
    pub async fn send_bytes(&mut self, payload: &[u8]) -> AgentResult<()> {
        self.ensure_writable()?;
        let line = encode_frame_bytes(payload, self.max_frame_bytes)?;
        self.write_line(&line).await
    }

    /// 序列化一个 JSON 值再写出。同样：编码失败不毒化，写失败才毒化。
    pub async fn send_json(&mut self, value: &serde_json::Value) -> AgentResult<()> {
        self.ensure_writable()?;
        let line = encode_frame(value, self.max_frame_bytes)?;
        self.write_line(&line).await
    }

    /// Write a frame that the protocol layer already capped (`encode_frame`).
    /// Encoding already happened, so this only fails on a poisoned session
    /// or a partial write.
    pub async fn send_encoded_line(&mut self, line: &[u8]) -> AgentResult<()> {
        self.ensure_writable()?;
        self.write_line(line).await
    }

    fn ensure_writable(&self) -> AgentResult<()> {
        if let Some(reason) = &self.poisoned {
            return Err(AgentError::InvalidRequest(format!(
                "session poisoned: {reason}"
            )));
        }
        Ok(())
    }

    async fn write_line(&mut self, line: &[u8]) -> AgentResult<()> {
        if let Err(error) = self.writer.write_all(line).await {
            self.poison(format!("write failed: {error}"));
            return Err(AgentError::Io(error.to_string()));
        }
        if let Err(error) = self.writer.flush().await {
            self.poison(format!("flush failed: {error}"));
            return Err(AgentError::Io(error.to_string()));
        }
        Ok(())
    }
}

#[async_trait]
impl<R, W> DuplexTransport for FramedProtocolSession<R, W>
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    fn is_poisoned(&self) -> bool {
        FramedProtocolSession::is_poisoned(self)
    }

    fn poison_reason(&self) -> Option<&str> {
        FramedProtocolSession::poison_reason(self)
    }

    fn poison(&mut self, reason: String) {
        FramedProtocolSession::poison(self, reason);
    }

    async fn recv(&mut self) -> Result<Vec<u8>, FrameError> {
        FramedProtocolSession::recv(self).await
    }

    async fn send_encoded_line(&mut self, line: &[u8]) -> AgentResult<()> {
        FramedProtocolSession::send_encoded_line(self, line).await
    }
}

impl<S> FramedProtocolSession<BufReader<ReadHalf<S>>, WriteHalf<S>>
where
    S: AsyncRead + AsyncWrite,
{
    /// 把一条双工字节流拆成读写端。继承匿名管道、测试用 `duplex`、
    /// 以及以后的 Named Pipe/UDS 都可以走这里；传输身份仍不是授权。
    pub fn from_stream(stream: S, max_frame_bytes: usize) -> AgentResult<Self> {
        let (reader, writer) = tokio::io::split(stream);
        Self::new(BufReader::new(reader), writer, max_frame_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn zero_bound_is_rejected() {
        let error = match FramedProtocolSession::from_stream(tokio::io::duplex(64).1, 0) {
            Ok(_) => panic!("zero bound must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("positive"));
    }

    #[tokio::test]
    async fn duplex_echo_preserves_coalesced_frames() {
        let (client, server) = tokio::io::duplex(4096);
        let mut server = FramedProtocolSession::from_stream(server, 1024).unwrap();
        let mut client = FramedProtocolSession::from_stream(client, 1024).unwrap();

        let server_task = tokio::spawn(async move {
            let first = server.recv().await.unwrap();
            server.send_bytes(&first).await.unwrap();
            let second = server.recv().await.unwrap();
            server.send_bytes(&second).await.unwrap();
            server
        });

        client.send_json(&json!({"seq": 1})).await.unwrap();
        client.send_json(&json!({"seq": 2})).await.unwrap();
        let first = client.recv().await.unwrap();
        let second = client.recv().await.unwrap();
        assert_eq!(first, br#"{"seq":1}"#);
        assert_eq!(second, br#"{"seq":2}"#);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn inbound_oversize_poisons_the_session() {
        let (mut client, server) = tokio::io::duplex(8192);
        let mut server = FramedProtocolSession::from_stream(server, 32).unwrap();
        let huge = format!("{}\n", "x".repeat(64));
        client.write_all(huge.as_bytes()).await.unwrap();
        client.flush().await.unwrap();

        let error = server.recv().await.unwrap_err();
        assert!(matches!(error.kind, FrameErrorKind::Oversize { limit: 32 }));
        assert!(server.is_poisoned());
        let poisoned = server.recv().await.unwrap_err();
        assert!(matches!(poisoned.kind, FrameErrorKind::Poisoned { .. }));
        let send = server.send_json(&json!({"ok": true})).await.unwrap_err();
        assert!(send.to_string().contains("poisoned"));
    }

    #[tokio::test]
    async fn partial_eof_poisons_clean_eof_does_not() {
        let (client, server) = tokio::io::duplex(1024);
        let mut server = FramedProtocolSession::from_stream(server, 1024).unwrap();
        drop(client);
        let eof = server.recv().await.unwrap_err();
        assert_eq!(eof.kind, FrameErrorKind::Eof);
        assert!(!server.is_poisoned());

        let (mut client, server) = tokio::io::duplex(1024);
        let mut server = FramedProtocolSession::from_stream(server, 1024).unwrap();
        client.write_all(b"{\"a\":1").await.unwrap();
        client.flush().await.unwrap();
        drop(client);
        let partial = server.recv().await.unwrap_err();
        assert_eq!(partial.kind, FrameErrorKind::PartialEof);
        assert!(server.is_poisoned());
    }

    #[tokio::test]
    async fn outbound_oversize_does_not_poison() {
        let (_client, server) = tokio::io::duplex(1024);
        let mut server = FramedProtocolSession::from_stream(server, 16).unwrap();
        let error = server
            .send_json(&json!({"payload": "0123456789abcdef"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bound"));
        assert!(
            !server.is_poisoned(),
            "a rejected outbound encode must leave the session usable"
        );
        server.send_json(&json!({"ok": 1})).await.unwrap();
    }

    async fn echo_encoded(transport: &mut impl DuplexTransport, max_frame_bytes: usize) {
        let frame = transport.recv().await.unwrap();
        let line = encode_frame_bytes(&frame, max_frame_bytes).unwrap();
        transport.send_encoded_line(&line).await.unwrap();
    }

    #[tokio::test]
    async fn duplex_backend_satisfies_duplex_transport() {
        let (client, server) = tokio::io::duplex(4096);
        let mut server = FramedProtocolSession::from_stream(server, 1024).unwrap();
        let mut client = FramedProtocolSession::from_stream(client, 1024).unwrap();
        let server_task = tokio::spawn(async move {
            echo_encoded(&mut server, 1024).await;
            assert!(!DuplexTransport::is_poisoned(&server));
            server
        });

        let line = encode_frame(&json!({"ping": true}), 1024).unwrap();
        DuplexTransport::send_encoded_line(&mut client, &line)
            .await
            .unwrap();
        let reply = DuplexTransport::recv(&mut client).await.unwrap();
        assert_eq!(reply, br#"{"ping":true}"#);
        server_task.await.unwrap();

        DuplexTransport::poison(&mut client, "test fence".into());
        assert!(DuplexTransport::is_poisoned(&client));
        assert_eq!(DuplexTransport::poison_reason(&client), Some("test fence"));
        let reuse = DuplexTransport::recv(&mut client).await.unwrap_err();
        assert!(matches!(reuse.kind, FrameErrorKind::Poisoned { .. }));
    }
}
