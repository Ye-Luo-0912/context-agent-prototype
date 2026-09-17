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

impl StreamSource {
    /// Independent decode slot per stream: a fragment of one stream never
    /// completes a character for the other.
    fn decode_slot(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

pub(crate) struct OutputChunk {
    bytes: Vec<u8>,
    /// This fragment ends the current logical line (newline or pipe EOF).
    line_end: bool,
    /// The source contained a newline after `bytes`; preserve it in artifact.
    newline: bool,
    /// This fragment follows an earlier fragment of the same overlong line.
    continued: bool,
    /// Empty end-of-stream marker from the pump: lets the capture flush
    /// each stream's retained UTF-8 decode suffix with lossy semantics.
    eof: bool,
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

    fn into_parts(self) -> (StreamSource, OutputChunk) {
        match self {
            Self::Stdout(output) => (StreamSource::Stdout, output),
            Self::Stderr(output) => (StreamSource::Stderr, output),
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
                if !send_output(&tx, source, bytes, true, true, continued, false).await {
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
                if !send_output(&tx, source, bytes, false, false, continued, false).await {
                    return;
                }
                continued = true;
            }
            pending.push(byte);
        }
    }

    if !pending.is_empty() {
        let _receiver_open = send_output(&tx, source, pending, true, false, continued, false).await;
    }
    // End-of-stream marker for THIS stream: the capture flushes its
    // retained UTF-8 decode suffix with lossy semantics. The marker
    // carries no bytes and is dropped silently if the receiver is gone.
    let _ = send_output(&tx, source, Vec::new(), false, false, false, true).await;
}

async fn send_output(
    tx: &mpsc::Sender<StreamChunk>,
    source: StreamSource,
    bytes: Vec<u8>,
    line_end: bool,
    newline: bool,
    continued: bool,
    eof: bool,
) -> bool {
    debug_assert!(bytes.len() <= MAX_STREAM_ITEM_BYTES);
    tx.send(StreamChunk::from_output(
        source,
        OutputChunk {
            bytes,
            line_end,
            newline,
            continued,
            eof,
        },
    ))
    .await
    .is_ok()
}

/// An at-most-3-byte unfinished UTF-8 sequence retained between fragments
/// of ONE stream. Decode state is per stream, so a fragment of one stream
/// can never complete the other's character.
#[derive(Default)]
struct Utf8Tail {
    bytes: [u8; 3],
    len: usize,
}

impl Utf8Tail {
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn retain(&mut self, rest: &[u8]) {
        debug_assert!(rest.len() <= 3, "an unfinished sequence is at most 3 bytes");
        self.bytes[..rest.len()].copy_from_slice(rest);
        self.len = rest.len();
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

/// Decode `bytes` (after this stream's retained suffix) up to the last
/// complete character boundary, per `std::str::from_utf8` semantics:
/// complete sequences pass through verbatim, an invalid sequence renders
/// one U+FFFD and is consumed, and an unfinished suffix (1-3 bytes at the
/// end) is retained for the next fragment of the same stream.
fn decode_utf8_incremental(tail: &mut Utf8Tail, bytes: &[u8]) -> String {
    let mut text = String::new();
    if tail.is_empty() {
        decode_utf8_into(&mut text, tail, bytes);
    } else {
        let mut merged = Vec::with_capacity(tail.len + bytes.len());
        merged.extend_from_slice(tail.as_slice());
        merged.extend_from_slice(bytes);
        decode_utf8_into(&mut text, tail, &merged);
    }
    text
}

fn decode_utf8_into(text: &mut String, tail: &mut Utf8Tail, rest: &[u8]) {
    let mut rest = rest;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                tail.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                // from_utf8 guarantees [..valid_up_to] is valid UTF-8.
                text.push_str(std::str::from_utf8(&rest[..valid_up_to]).unwrap_or_default());
                match error.error_len() {
                    Some(consumed) => {
                        // A genuinely invalid sequence: one replacement,
                        // then keep decoding behind it.
                        text.push('\u{FFFD}');
                        rest = &rest[valid_up_to + consumed..];
                        if rest.is_empty() {
                            tail.clear();
                            break;
                        }
                    }
                    None => {
                        // Incomplete sequence at the end: hold it for the
                        // next fragment of this stream.
                        tail.retain(&rest[valid_up_to..]);
                        break;
                    }
                }
            }
        }
    }
}

/// A retained suffix that can never complete (end of stream, or the line
/// boundary it belonged to) ends as `String::from_utf8_lossy` would: an
/// unfinished sequence is exactly one U+FFFD.
fn flush_utf8_tail(tail: &mut Utf8Tail) -> String {
    if tail.is_empty() {
        return String::new();
    }
    let text = String::from_utf8_lossy(tail.as_slice()).into_owned();
    tail.clear();
    text
}

/// Bounded model tail plus checked raw-output/artifact accounting.
pub(crate) struct StreamCapture {
    tail: VecDeque<String>,
    total_chunks: usize,
    total_lines: usize,
    total_bytes: usize,
    artifact_bytes: usize,
    artifact_truncated: bool,
    /// Per-stream incremental UTF-8 decode state: each stream holds at
    /// most a 3-byte unfinished sequence between fragments, and the two
    /// streams never share state.
    decode: [Utf8Tail; 2],
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
            decode: [Utf8Tail::default(), Utf8Tail::default()],
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
        let (source, output) = item.into_parts();
        debug_assert!(output.bytes.len() <= MAX_STREAM_ITEM_BYTES);

        // The pump's empty end-of-stream marker: flush THIS stream's
        // retained decode suffix with lossy semantics. The raw bytes were
        // already captured with their original fragments, so the marker
        // adds no bytes and writes nothing; a flushed replacement text is
        // counted together with its tail entry so the omission arithmetic
        // stays exact.
        if output.eof {
            debug_assert!(
                output.bytes.is_empty(),
                "the end-of-stream marker carries no bytes"
            );
            let flushed = flush_utf8_tail(&mut self.decode[source.decode_slot()]);
            if !flushed.is_empty() {
                self.total_chunks = self.total_chunks.saturating_add(1);
                if self.tail.len() >= BUFFER_LINES {
                    self.tail.pop_front();
                }
                self.tail.push_back(flushed);
            }
            return Ok(false);
        }

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

        // Model-facing text decodes across fragments per stream: complete
        // characters out now, an at-most-3-byte unfinished suffix held
        // for the next fragment of the SAME stream.
        let slot = source.decode_slot();
        let mut display = decode_utf8_incremental(&mut self.decode[slot], &output.bytes);
        if output.newline {
            // A line boundary can never complete a partial sequence — the
            // bytes after it belong to the next line — so a trailing
            // partial sequence of a newline-terminated fragment is
            // finished here, not carried across the break.
            display.push_str(&flush_utf8_tail(&mut self.decode[slot]));
        }
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
            let (_source, output) = item.into_parts();
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
                        eof: false,
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

    // -- H5 regressions (eleventh batch): incremental UTF-8 decoding ----------
    //
    // The pump splits long lines at raw-byte boundaries that need not fall
    // on character boundaries. The model-facing tail must decode across
    // fragments per stream (each stream keeps its own at most-3-byte
    // unfinished suffix), flush the suffix at end-of-stream with lossy
    // semantics, and leave the raw artifact byte-for-byte identical.

    /// Drive the REAL pump for one stream and the REAL capture: every
    /// chunk the pump emits is recorded into a temp-file artifact.
    /// Returns the capture and the artifact bytes.
    async fn pump_and_capture(input: &[u8], stderr: bool) -> (StreamCapture, Vec<u8>) {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel(32);
        if stderr {
            spawn_stderr_reader(reader, tx.clone());
        } else {
            spawn_stdout_reader(reader, tx.clone());
        }
        drop(tx);
        let input_owned = input.to_vec();
        let writer_task = tokio::spawn(async move {
            writer.write_all(&input_owned).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.log");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut artifact = BufWriter::new(file);
        let mut capture = StreamCapture::new();
        while let Some(chunk) = rx.recv().await {
            capture.record(chunk, &mut artifact).await.unwrap();
        }
        artifact.flush().await.unwrap();
        writer_task.await.unwrap();
        let bytes = tokio::fs::read(&path).await.unwrap();
        (capture, bytes)
    }

    /// H5: a multi-byte character split at the 4000-byte fragment
    /// boundary — 2/3/4-byte chars starting at every offset 3997..=4001 —
    /// must reach the model tail as the original character, never as
    /// U+FFFD, while the raw artifact stays byte-for-byte identical.
    #[tokio::test]
    async fn multibyte_chars_split_at_the_item_boundary_reach_the_model_tail_intact() {
        for start in 3997_usize..=4001 {
            for (label, ch) in [("2-byte", "é"), ("3-byte", "界"), ("4-byte", "\u{1F600}")] {
                let mut line = String::new();
                line.push_str(&"a".repeat(start));
                line.push_str(ch);
                line.push_str(&"z".repeat(64));
                line.push('\n');
                let input = line.as_bytes();
                let (capture, artifact) = pump_and_capture(input, false).await;
                let tail = capture.model_tail();
                assert_eq!(
                    tail.matches('\u{FFFD}').count(),
                    0,
                    "{label} char split at {start} must not be corrupted: {tail:?}"
                );
                assert!(
                    tail.contains(ch),
                    "{label} char split at {start} must reach the model tail intact: {tail:?}"
                );
                assert_eq!(
                    artifact, input,
                    "the raw artifact must be byte-for-byte identical"
                );
                assert_eq!(capture.total_bytes(), input.len());
            }
        }
    }

    /// H5: an unfinished multi-byte suffix at an unterminated end of
    /// stream is flushed exactly once, with from_utf8_lossy semantics
    /// (one U+FFFD for the partial sequence).
    #[tokio::test]
    async fn no_newline_eof_flushes_the_retained_suffix_lossily() {
        let mut input = b"hello".to_vec();
        input.extend_from_slice(&"界".as_bytes()[..2]);
        let (capture, artifact) = pump_and_capture(&input, false).await;
        let tail = capture.model_tail();
        assert!(
            tail.starts_with("hello"),
            "the complete prefix must be unaffected: {tail:?}"
        );
        assert_eq!(
            tail.matches('\u{FFFD}').count(),
            1,
            "the partial sequence becomes exactly one U+FFFD at EOF: {tail:?}"
        );
        assert_eq!(artifact, input, "raw artifact bytes must be identical");
        assert_eq!(capture.total_bytes(), input.len());
    }

    /// H5: stdout and stderr decode suffixes are independent. Each
    /// stream's first 4000-byte fragment ends mid-character; the OTHER
    /// stream's completing fragment is recorded in between. The pending
    /// bytes of one stream must never complete a character from the
    /// other's bytes.
    #[tokio::test]
    async fn stdout_and_stderr_decode_suffixes_stay_independent() {
        let stdout_input = format!("{}中\n", "a".repeat(3999));
        let stderr_input = format!("{}界\n", "e".repeat(3999));
        let (mut so_writer, so_reader) = tokio::io::duplex(64 * 1024);
        let (mut se_writer, se_reader) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel(32);
        spawn_stdout_reader(so_reader, tx.clone());
        spawn_stderr_reader(se_reader, tx.clone());
        drop(tx);

        // Feed the first 4000 bytes of each stream (each ends mid-char),
        // then stderr's remainder, then stdout's: a stderr fragment is
        // recorded while stdout's suffix is still pending.
        so_writer
            .write_all(&stdout_input.as_bytes()[..4000])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        se_writer
            .write_all(&stderr_input.as_bytes()[..4000])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        se_writer
            .write_all(&stderr_input.as_bytes()[4000..])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        so_writer
            .write_all(&stdout_input.as_bytes()[4000..])
            .await
            .unwrap();
        drop(so_writer);
        drop(se_writer);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interleaved.log");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut artifact = BufWriter::new(file);
        let mut capture = StreamCapture::new();
        let mut stdout_recon = Vec::new();
        let mut stderr_recon = Vec::new();
        while let Some(chunk) = rx.recv().await {
            match &chunk {
                StreamChunk::Stdout(output) => {
                    stdout_recon.extend_from_slice(&output.bytes);
                    if output.newline {
                        stdout_recon.push(b'\n');
                    }
                }
                StreamChunk::Stderr(output) => {
                    stderr_recon.extend_from_slice(&output.bytes);
                    if output.newline {
                        stderr_recon.push(b'\n');
                    }
                }
            }
            capture.record(chunk, &mut artifact).await.unwrap();
        }
        artifact.flush().await.unwrap();

        assert_eq!(
            stdout_recon,
            stdout_input.as_bytes(),
            "stdout fragments must rebuild the original bytes"
        );
        assert_eq!(
            stderr_recon,
            stderr_input.as_bytes(),
            "stderr fragments must rebuild the original bytes"
        );
        let tail = capture.model_tail();
        assert_eq!(
            tail.matches('\u{FFFD}').count(),
            0,
            "no stream may corrupt the other's characters: {tail:?}"
        );
        assert!(
            tail.contains('中') && tail.contains('界'),
            "both streams' characters must reach the model tail intact: {tail:?}"
        );
        assert_eq!(
            capture.total_bytes(),
            stdout_input.len() + stderr_input.len()
        );
    }

    /// H5: genuinely invalid UTF-8 replaces exactly the invalid sequences
    /// — [0xff, 0xfe] becomes exactly two U+FFFD — whether or not the
    /// stream ends in a newline, and the artifact keeps the raw bytes.
    #[tokio::test]
    async fn invalid_utf8_fragments_replace_exactly_the_invalid_sequences() {
        let (capture, artifact) = pump_and_capture(&[0xff, 0xfe, b'\n'], false).await;
        let tail = capture.model_tail();
        assert_eq!(
            tail.matches('\u{FFFD}').count(),
            2,
            "each invalid byte is one replacement: {tail:?}"
        );
        assert_eq!(artifact, &[0xff, 0xfe, b'\n']);

        let (capture, artifact) = pump_and_capture(&[0xff, 0xfe], false).await;
        assert_eq!(
            capture.model_tail().matches('\u{FFFD}').count(),
            2,
            "the unterminated case replaces exactly as well"
        );
        assert_eq!(artifact, &[0xff, 0xfe]);
    }

    /// H5: ~9 KB of legal mixed-width output crosses two fragment
    /// boundaries at arbitrary character positions and stays
    /// replacement-free end to end, including an unterminated tail whose
    /// last character is multi-byte. The leading 界 shifts the 10-byte
    /// piece cycle so the 4000/8000 boundaries land INSIDE the 4-byte
    /// character rather than between pieces.
    #[tokio::test]
    async fn legal_multibyte_streams_stay_replacement_free_end_to_end() {
        let mut line = String::new();
        line.push('界');
        let pieces = ["a", "é", "界", "\u{1F600}"];
        while line.len() < 9 * 1024 {
            for piece in pieces {
                line.push_str(piece);
            }
        }
        line.push('\n');
        // Unterminated tail: the final 界 completes only at the EOF flush.
        let mut input = line.as_bytes().to_vec();
        input.extend_from_slice("界-tail".as_bytes());

        let (capture, artifact) = pump_and_capture(&input, false).await;
        let tail = capture.model_tail();
        assert_eq!(
            tail.matches('\u{FFFD}').count(),
            0,
            "legal input must produce zero replacements anywhere: {tail:?}"
        );
        assert!(tail.contains("界-tail"));
        assert_eq!(artifact, input, "raw artifact bytes must be identical");
        assert_eq!(capture.total_bytes(), input.len());
    }
}
