//! Shared bounded stream handling for the process-executing tools.
//!
//! Pipe readers never use `lines()`: one missing newline would let that API
//! grow a `String` without bound. Instead, fixed-size byte buffers emit
//! bounded line fragments through a bounded channel. Raw output is drained
//! for the lifetime of the process, while artifact capture stops at a hard
//! per-invocation limit and the model sees only a bounded tail.

use std::collections::VecDeque;

use agent_contracts::{AgentError, AgentResult};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

pub(crate) const MODEL_OUTPUT_CHARS: usize = 12_000;
pub(crate) const BUFFER_LINES: usize = 200;

/// Hard byte bound for one channel item. Lossy UTF-8 conversion happens only
/// after receipt, so invalid input cannot inflate the channel allocation.
pub(crate) const MAX_STREAM_ITEM_BYTES: usize = 4_000;

/// Hard limit for the captured raw prefix of one process invocation/session.
/// Readers continue draining after this limit so a full pipe cannot deadlock
/// the child or defeat timeout/cancellation handling.
pub(crate) const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;

const READ_BUFFER_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy)]
enum StreamSource {
    Stdout,
    Stderr,
}

pub(crate) struct OutputChunk {
    bytes: Vec<u8>,
    /// This fragment ends the current logical line (newline or pipe EOF).
    line_end: bool,
    /// The source contained a newline after `bytes`; preserve it in artifact.
    newline: bool,
    /// This fragment follows an earlier fragment of the same overlong line.
    continued: bool,
}

pub(crate) enum StreamChunk {
    Stdout(OutputChunk),
    Stderr(OutputChunk),
}

impl StreamChunk {
    fn from_output(source: StreamSource, output: OutputChunk) -> Self {
        match source {
            StreamSource::Stdout => Self::Stdout(output),
            StreamSource::Stderr => Self::Stderr(output),
        }
    }

    fn into_output(self) -> OutputChunk {
        match self {
            Self::Stdout(output) | Self::Stderr(output) => output,
        }
    }
}

/// Spawn a bounded stdout reader. The detached task owns only the pipe and a
/// bounded sender; dropping the receiver makes it exit promptly.
pub(crate) fn spawn_stdout_reader<R>(reader: R, tx: mpsc::Sender<StreamChunk>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    spawn_reader(reader, tx, StreamSource::Stdout);
}

/// Spawn a bounded stderr reader. See [`spawn_stdout_reader`].
pub(crate) fn spawn_stderr_reader<R>(reader: R, tx: mpsc::Sender<StreamChunk>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    spawn_reader(reader, tx, StreamSource::Stderr);
}

fn spawn_reader<R>(reader: R, tx: mpsc::Sender<StreamChunk>, source: StreamSource)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    // Dropping a Tokio JoinHandle detaches the task. Its only possible wait is
    // pipe IO or bounded-channel backpressure; both end when the child/receiver
    // is dropped.
    drop(tokio::spawn(async move {
        pump_stream(reader, tx, source).await;
    }));
}

async fn pump_stream<R>(mut reader: R, tx: mpsc::Sender<StreamChunk>, source: StreamSource)
where
    R: AsyncRead + Unpin,
{
    let mut read_buffer = [0_u8; READ_BUFFER_BYTES];
    let mut pending = Vec::with_capacity(MAX_STREAM_ITEM_BYTES);
    let mut continued = false;

    loop {
        let read = match reader.read(&mut read_buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            // Preserve any bytes already read; the old line reader also ended
            // the stream on an IO failure. A later protocol layer can carry a
            // typed pipe error without making output memory unbounded.
            Err(_) => break,
        };

        for &byte in &read_buffer[..read] {
            if byte == b'\n' {
                let bytes =
                    std::mem::replace(&mut pending, Vec::with_capacity(MAX_STREAM_ITEM_BYTES));
                if !send_output(&tx, source, bytes, true, true, continued).await {
                    return;
                }
                continued = false;
                continue;
            }

            // Wait for the next byte before splitting an exactly-full fragment:
            // if that byte is a newline, the full fragment is still one line.
            if pending.len() == MAX_STREAM_ITEM_BYTES {
                let bytes =
                    std::mem::replace(&mut pending, Vec::with_capacity(MAX_STREAM_ITEM_BYTES));
                if !send_output(&tx, source, bytes, false, false, continued).await {
                    return;
                }
                continued = true;
            }
            pending.push(byte);
        }
    }

    if !pending.is_empty() {
        let _receiver_open = send_output(&tx, source, pending, true, false, continued).await;
    }
}

async fn send_output(
    tx: &mpsc::Sender<StreamChunk>,
    source: StreamSource,
    bytes: Vec<u8>,
    line_end: bool,
    newline: bool,
    continued: bool,
) -> bool {
    debug_assert!(bytes.len() <= MAX_STREAM_ITEM_BYTES);
    tx.send(StreamChunk::from_output(
        source,
        OutputChunk {
            bytes,
            line_end,
            newline,
            continued,
        },
    ))
    .await
    .is_ok()
}

/// Bounded model tail plus checked raw-output/artifact accounting.
pub(crate) struct StreamCapture {
    tail: VecDeque<String>,
    total_chunks: usize,
    total_lines: usize,
    total_bytes: usize,
    artifact_bytes: usize,
    artifact_truncated: bool,
}

impl StreamCapture {
    pub(crate) fn new() -> Self {
        Self {
            tail: VecDeque::with_capacity(BUFFER_LINES + 1),
            total_chunks: 0,
            total_lines: 0,
            total_bytes: 0,
            artifact_bytes: 0,
            artifact_truncated: false,
        }
    }

    /// Record one bounded fragment. Once the artifact prefix reaches its hard
    /// limit, this method keeps accounting/model-tail work but performs no
    /// further writes, allowing the caller to continue draining the pipe.
    pub(crate) async fn record<W>(
        &mut self,
        item: StreamChunk,
        artifact: &mut W,
    ) -> AgentResult<bool>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let output = item.into_output();
        debug_assert!(output.bytes.len() <= MAX_STREAM_ITEM_BYTES);

        let raw_bytes = output
            .bytes
            .len()
            .saturating_add(usize::from(output.newline));
        self.total_bytes = self.total_bytes.saturating_add(raw_bytes);
        self.total_chunks = self.total_chunks.saturating_add(1);
        if output.line_end {
            self.total_lines = self.total_lines.saturating_add(1);
        }

        let remaining = MAX_ARTIFACT_BYTES.saturating_sub(self.artifact_bytes);
        let data_bytes = remaining.min(output.bytes.len());
        if data_bytes > 0 {
            artifact
                .write_all(&output.bytes[..data_bytes])
                .await
                .map_err(|e| AgentError::Io(format!("append artifact: {e}")))?;
        }
        let mut written = data_bytes;
        if output.newline && remaining > data_bytes {
            artifact
                .write_all(b"\n")
                .await
                .map_err(|e| AgentError::Io(format!("append artifact: {e}")))?;
            written = written.saturating_add(1);
        }
        self.artifact_bytes = self.artifact_bytes.saturating_add(written);
        if written < raw_bytes {
            self.artifact_truncated = true;
        }

        let mut display = String::from_utf8_lossy(&output.bytes).into_owned();
        if output.newline && display.ends_with('\r') {
            display.pop();
        }
        if output.continued {
            display.insert_str(0, "...[line continued] ");
        }
        if !output.line_end {
            display.push_str(" ...[line continues]");
        }
        if self.tail.len() >= BUFFER_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(display);

        Ok(output.line_end)
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.total_lines
    }

    pub(crate) fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub(crate) fn artifact_bytes(&self) -> usize {
        self.artifact_bytes
    }

    pub(crate) fn artifact_truncated(&self) -> bool {
        self.artifact_truncated
    }

    /// Render the bounded tail. `total_chunks` (not logical lines) determines
    /// omission because one hostile no-newline line may occupy many fragments.
    pub(crate) fn model_tail(&self) -> String {
        let omitted = self.total_chunks.saturating_sub(self.tail.len());
        let mut model_content = self.tail.iter().cloned().collect::<Vec<_>>().join("\n");
        if model_content.chars().count() > MODEL_OUTPUT_CHARS {
            model_content = tail_chars(&model_content, MODEL_OUTPUT_CHARS);
        }
        if omitted > 0 {
            model_content = format!(
                "[{} output chunks total; {omitted} omitted]\n{model_content}",
                self.total_chunks
            );
        }
        model_content
    }
}

/// The bounded model-facing tail of a large output.
pub(crate) fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    format!(
        "...[{} chars omitted; showing tail]\n{}",
        skip,
        text.chars().skip(skip).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufWriter;

    #[tokio::test]
    async fn reader_bounds_items_and_preserves_long_invalid_output() {
        let mut input = vec![0xff; MAX_STREAM_ITEM_BYTES * 3 + 17];
        input.push(b'\n');
        input.extend_from_slice(b"tail-without-newline");

        let (mut writer, reader) = tokio::io::duplex(1024);
        let expected = input.clone();
        let writer_task = tokio::spawn(async move {
            writer.write_all(&input).await.unwrap();
        });

        let (tx, mut rx) = mpsc::channel(2);
        spawn_stdout_reader(reader, tx.clone());
        drop(tx);

        let mut rebuilt = Vec::new();
        while let Some(item) = rx.recv().await {
            let output = item.into_output();
            assert!(output.bytes.len() <= MAX_STREAM_ITEM_BYTES);
            rebuilt.extend_from_slice(&output.bytes);
            if output.newline {
                rebuilt.push(b'\n');
            }
        }
        writer_task.await.unwrap();
        assert_eq!(rebuilt, expected);
    }

    #[tokio::test]
    async fn artifact_capture_stops_at_limit_but_accounting_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded.log");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut artifact = BufWriter::new(file);
        let mut capture = StreamCapture::new();
        let chunks = MAX_ARTIFACT_BYTES / MAX_STREAM_ITEM_BYTES + 3;

        for _ in 0..chunks {
            capture
                .record(
                    StreamChunk::Stdout(OutputChunk {
                        bytes: vec![b'x'; MAX_STREAM_ITEM_BYTES],
                        line_end: false,
                        newline: false,
                        continued: true,
                    }),
                    &mut artifact,
                )
                .await
                .unwrap();
        }
        artifact.flush().await.unwrap();

        assert!(capture.total_bytes() > MAX_ARTIFACT_BYTES);
        assert_eq!(capture.artifact_bytes(), MAX_ARTIFACT_BYTES);
        assert!(capture.artifact_truncated());
        assert_eq!(
            tokio::fs::metadata(path).await.unwrap().len(),
            MAX_ARTIFACT_BYTES as u64
        );
    }
}
