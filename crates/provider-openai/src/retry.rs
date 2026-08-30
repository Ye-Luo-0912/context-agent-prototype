//! Generic retry/backoff wrapper for any `ModelTransport`.
//!
//! Typed retryable transport errors (network failures, timeouts, 5xx, 429,
//! and gateway-wrapped upstream 400 `Upstream request failed`) are retried,
//! and so is a model-emitted tool call whose argument JSON is malformed
//! (`MalformedToolCall`): that is transient output noise the model can fix
//! on re-issue, and in buffering mode nothing from the rejected stream
//! reaches the sink. Wire/protocol damage (`MalformedEvent`), auth errors,
//! and genuine provider-level rejections fail immediately. The backoff
//! yields to the request's cancellation token, so a cancelled request
//! aborts instead of sleeping out the wait.
//!
//! Streaming is retryable only while no chunk has crossed the sink's explicit
//! replay barrier. A sink may consume protocol-internal tool-call deltas
//! without publishing them; text already shown to a user remains irreversible.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, SystemTime};

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ModelCapabilities, ModelChunk, ModelEventSink,
    ModelOutput, ModelProtocolErrorKind, ModelRequest, ModelTransport, RetryAfterMillis,
};
use async_trait::async_trait;

const DEFAULT_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const MAX_BUFFERED_STREAM_CHUNKS: usize = 16_384;
/// A malformed tool-call body is a model-format incident, not an outage.
/// Permit one immediate regeneration, then surface persistent failure instead
/// of spending the transport retry budget and exponential backoff.
const MAX_TOOL_FORMAT_ATTEMPTS: u32 = 2;
type JitterFn = dyn Fn(Duration, u32) -> Duration + Send + Sync;

struct RetrySchedule {
    base_delay: Duration,
    max_delay: Duration,
    jitter: Arc<JitterFn>,
}

impl RetrySchedule {
    fn new(base_delay: Duration) -> Self {
        Self {
            base_delay,
            max_delay: DEFAULT_MAX_RETRY_DELAY,
            jitter: Arc::new(default_jitter),
        }
    }

    fn delay(&self, retry_number: u32, error: &AgentError) -> Duration {
        let exponent = retry_number.saturating_sub(1);
        let multiplier = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let exponential = self
            .base_delay
            .checked_mul(multiplier)
            .unwrap_or(self.max_delay)
            .min(self.max_delay);
        let jittered = (self.jitter)(exponential, retry_number).min(self.max_delay);
        let retry_after = match error {
            AgentError::TransportRetryAfter { retry_after_ms, .. } => {
                Duration::from_millis(u64::from(retry_after_ms.get())).min(self.max_delay)
            }
            _ => Duration::ZERO,
        };
        jittered.max(retry_after)
    }
}

// Equal jitter avoids synchronized retries while preserving half of the
// exponential delay. Tests inject a deterministic function through
// `with_jitter`, so timing assertions never depend on this process-wide state.
fn default_jitter(delay: Duration, retry_number: u32) -> Duration {
    static STATE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
    if delay.is_zero() {
        return delay;
    }
    let clock_bits = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let mut sample = STATE.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
        ^ u64::from(retry_number)
        ^ clock_bits;
    sample ^= sample >> 30;
    sample = sample.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    sample ^= sample >> 27;
    sample = sample.wrapping_mul(0x94d0_49bb_1331_11eb);
    sample ^= sample >> 31;

    let floor = delay / 2;
    let span_nanos = (delay - floor).as_nanos().min(u64::MAX as u128) as u64;
    let extra_nanos = sample % span_nanos.saturating_add(1);
    floor
        .checked_add(Duration::from_nanos(extra_nanos))
        .unwrap_or(delay)
        .min(delay)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryClass {
    Transport,
    ToolCallFormat,
}

fn retry_class(error: &AgentError) -> Option<RetryClass> {
    match error {
        AgentError::Transport {
            retryable: true, ..
        }
        | AgentError::TransportRetryAfter { .. } => Some(RetryClass::Transport),
        AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedToolCall,
            ..
        } => Some(RetryClass::ToolCallFormat),
        _ => None,
    }
}

#[cfg(test)]
fn retryable(error: &AgentError) -> bool {
    retry_class(error).is_some()
}

fn retry_attempt_limit(class: RetryClass, configured: u32) -> u32 {
    match class {
        RetryClass::Transport => configured.max(1),
        RetryClass::ToolCallFormat => configured.clamp(1, MAX_TOOL_FORMAT_ATTEMPTS),
    }
}

/// Independent retry credits with one aggregate ceiling. A format incident
/// cannot consume transport recovery (or vice versa), while alternating
/// failures still cannot create an unbounded call loop.
#[derive(Debug, Clone, Copy)]
struct RetryBudget {
    attempts: u32,
    transport_failures: u32,
    format_failures: u32,
    transport_attempt_limit: u32,
    format_attempt_limit: u32,
    total_attempt_limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryReservation {
    next_attempt: u32,
    total_attempt_limit: u32,
    class_retry_number: u32,
}

impl RetryBudget {
    fn new(configured: u32) -> Self {
        let transport_attempt_limit = retry_attempt_limit(RetryClass::Transport, configured);
        let format_attempt_limit = retry_attempt_limit(RetryClass::ToolCallFormat, configured);
        Self {
            attempts: 1,
            transport_failures: 0,
            format_failures: 0,
            transport_attempt_limit,
            format_attempt_limit,
            total_attempt_limit: transport_attempt_limit
                .saturating_add(format_attempt_limit.saturating_sub(1)),
        }
    }

    fn reserve_retry(&mut self, class: RetryClass) -> Option<RetryReservation> {
        let (failures, attempt_limit) = match class {
            RetryClass::Transport => (&mut self.transport_failures, self.transport_attempt_limit),
            RetryClass::ToolCallFormat => (&mut self.format_failures, self.format_attempt_limit),
        };
        *failures = failures.saturating_add(1);
        let class_retry_number = *failures;
        if class_retry_number >= attempt_limit || self.attempts >= self.total_attempt_limit {
            return None;
        }
        self.attempts = self.attempts.saturating_add(1);
        Some(RetryReservation {
            next_attempt: self.attempts,
            total_attempt_limit: self.total_attempt_limit,
            class_retry_number,
        })
    }
}

fn retry_delay(
    class: RetryClass,
    schedule: &RetrySchedule,
    attempt: u32,
    error: &AgentError,
) -> Duration {
    match class {
        RetryClass::Transport => schedule.delay(attempt, error),
        RetryClass::ToolCallFormat => Duration::ZERO,
    }
}

/// Short diagnostic label for the retry log line. Retries are real but
/// infrequent; the reason is otherwise swallowed by the retry loop, so it
/// is written to stderr (the eval harness redirects it into the run log).
/// Provider error bodies are deliberately omitted: durable typed evidence is
/// the destination, and intermediate bodies may contain sensitive data.
fn retry_reason_label(error: &AgentError) -> &'static str {
    match error {
        AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedToolCall,
            ..
        } => "malformed tool-call JSON",
        AgentError::TransportRetryAfter { .. }
        | AgentError::Transport {
            retryable: true, ..
        } => "retryable transport error",
        _ => "unexpected retryable error",
    }
}

fn log_retry(attempt_next: u32, max_attempts: u32, delay_ms: u64, error: &AgentError) {
    eprintln!(
        "[provider-openai] retrying model call: reason={} attempt={}/{} delay={}ms",
        retry_reason_label(error),
        attempt_next,
        max_attempts,
        delay_ms,
    );
}

pub struct RetryingTransport<T: ModelTransport> {
    inner: T,
    max_attempts: u32,
    schedule: RetrySchedule,
    /// Whole-response buffering mode: chunks are collected internally and
    /// only forwarded to the real sink after a successful attempt, so a
    /// retryable mid-stream failure can replay from scratch. Harnesses
    /// measure outcomes rather than render live deltas; interactive hosts
    /// keep the live mode where already-emitted output blocks a replay.
    buffering: bool,
}

impl<T: ModelTransport> RetryingTransport<T> {
    /// `max_attempts` is the transport-attempt ceiling. When it permits
    /// retries, one independent malformed-tool regeneration credit may raise
    /// the aggregate call ceiling to `max_attempts + 1`; the mixed budget is
    /// still hard-bounded and each class keeps its own credit.
    pub fn new(inner: T, max_attempts: u32, base_delay: Duration) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            schedule: RetrySchedule::new(base_delay),
            buffering: false,
        }
    }

    /// Buffering variant for outcome-measuring harnesses: any retryable
    /// transport failure is retried from scratch even after chunks were
    /// produced, because nothing reached the real sink until success.
    pub fn new_buffering(inner: T, max_attempts: u32, base_delay: Duration) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            schedule: RetrySchedule::new(base_delay),
            buffering: true,
        }
    }

    /// Cap every computed or provider-requested delay. The hard protocol
    /// bound still applies if a caller supplies a larger value.
    pub fn with_max_delay(mut self, max_delay: Duration) -> Self {
        let hard_max = Duration::from_millis(u64::from(RetryAfterMillis::MAX_MILLIS));
        self.schedule.max_delay = max_delay.min(hard_max);
        self
    }

    /// Inject a bounded jitter transform. The result is always clamped to
    /// `max_delay`; deterministic closures make retry schedules exact in
    /// tests and controlled experiments.
    pub fn with_jitter<J>(mut self, jitter: J) -> Self
    where
        J: Fn(Duration, u32) -> Duration + Send + Sync + 'static,
    {
        self.schedule.jitter = Arc::new(jitter);
        self
    }
}

/// Forwards chunks and records whether any successfully delivered chunk crossed
/// the sink's replay barrier. The sink, not the provider adapter, knows which
/// normalized chunks actually became externally visible.
struct EmissionTrackingSink<'a> {
    inner: &'a dyn ModelEventSink,
    emitted: &'a AtomicBool,
}

#[async_trait]
impl ModelEventSink for EmissionTrackingSink<'_> {
    fn creates_replay_barrier(&self, chunk: &ModelChunk) -> bool {
        self.inner.creates_replay_barrier(chunk)
    }

    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        let creates_barrier = self.inner.creates_replay_barrier(&chunk);
        if creates_barrier {
            // Fail closed before arbitrary sink code: a sink may publish and
            // then return an error, which is already irreversible.
            self.emitted.store(true, Ordering::Relaxed);
        }
        self.inner.on_chunk(chunk).await
    }
}

#[async_trait]
impl<T: ModelTransport> ModelTransport for RetryingTransport<T> {
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let (mut output, attempts) =
            retry(self.max_attempts, &self.schedule, &request.cancel, || {
                self.inner.complete(request.clone())
            })
            .await?;
        stamp_attempt_usage(&mut output, attempts);
        Ok(output)
    }

    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        if self.buffering {
            self.complete_stream_buffered(request, sink).await
        } else {
            self.complete_stream_live(request, sink).await
        }
    }
}

impl<T: ModelTransport> RetryingTransport<T> {
    /// Live mode: forward chunks as they arrive; a retryable failure after an
    /// irreversible chunk cannot be replayed into the same sink, so it is
    /// surfaced instead of retried. Protocol-internal chunks may remain below
    /// that boundary when the sink explicitly says so.
    async fn complete_stream_live(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        let emitted = AtomicBool::new(false);
        let mut budget = RetryBudget::new(self.max_attempts);
        loop {
            let tracking = EmissionTrackingSink {
                inner: sink,
                emitted: &emitted,
            };
            match self.inner.complete_stream(request.clone(), &tracking).await {
                Ok(mut output) => {
                    stamp_attempt_usage(&mut output, budget.attempts);
                    return Ok(output);
                }
                Err(error) => {
                    let Some(class) = retry_class(&error) else {
                        return Err(error);
                    };
                    if emitted.load(Ordering::Relaxed) {
                        // A stream that already emitted deltas cannot be
                        // replayed into the same sink: the live listener has
                        // no rewind, and a retry would duplicate the output.
                        return Err(error);
                    }
                    let Some(reservation) = budget.reserve_retry(class) else {
                        return Err(error);
                    };
                    let delay = retry_delay(
                        class,
                        &self.schedule,
                        reservation.class_retry_number,
                        &error,
                    );
                    log_retry(
                        reservation.next_attempt,
                        reservation.total_attempt_limit,
                        delay.as_millis().min(u64::MAX as u128) as u64,
                        &error,
                    );
                    sink.on_chunk(ModelChunk::Retrying {
                        attempt: reservation.next_attempt,
                        delay_ms: delay.as_millis().min(u64::MAX as u128) as u64,
                    })
                    .await?;
                    tokio::select! {
                        _ = request.cancel.cancelled() => return Err(AgentError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    /// Buffering mode: collect each attempt's chunks internally; only a
    /// successful attempt is forwarded to the real sink, so every retryable
    /// transport failure can replay from scratch without duplication.
    async fn complete_stream_buffered(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        let mut budget = RetryBudget::new(self.max_attempts);
        loop {
            let collected = BufferedSink::default();
            match self
                .inner
                .complete_stream(request.clone(), &collected)
                .await
            {
                Ok(mut output) => {
                    for chunk in collected.take()? {
                        sink.on_chunk(chunk).await?;
                    }
                    stamp_attempt_usage(&mut output, budget.attempts);
                    return Ok(output);
                }
                Err(error) => {
                    let Some(class) = retry_class(&error) else {
                        return Err(error);
                    };
                    let Some(reservation) = budget.reserve_retry(class) else {
                        return Err(error);
                    };
                    let delay = retry_delay(
                        class,
                        &self.schedule,
                        reservation.class_retry_number,
                        &error,
                    );
                    log_retry(
                        reservation.next_attempt,
                        reservation.total_attempt_limit,
                        delay.as_millis().min(u64::MAX as u128) as u64,
                        &error,
                    );
                    sink.on_chunk(ModelChunk::Retrying {
                        attempt: reservation.next_attempt,
                        delay_ms: delay.as_millis().min(u64::MAX as u128) as u64,
                    })
                    .await?;
                    tokio::select! {
                        _ = request.cancel.cancelled() => return Err(AgentError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}

/// Collects one attempt's chunks for the buffering mode. The wrapper is
/// generic, so it enforces its own chunk and resident-byte limits instead of
/// assuming that every inner transport has an equivalent stream boundary.
struct BufferedSink {
    state: std::sync::Mutex<BufferedState>,
    max_chunks: usize,
    max_bytes: usize,
}

#[derive(Default)]
struct BufferedState {
    chunks: Vec<ModelChunk>,
    bytes: usize,
    limit_error: Option<String>,
}

impl Default for BufferedSink {
    fn default() -> Self {
        Self::with_limits(MAX_BUFFERED_STREAM_CHUNKS, crate::DEFAULT_MAX_STREAM_BYTES)
    }
}

impl BufferedSink {
    fn with_limits(max_chunks: usize, max_bytes: usize) -> Self {
        Self {
            state: std::sync::Mutex::new(BufferedState::default()),
            max_chunks,
            max_bytes,
        }
    }

    fn take(&self) -> AgentResult<Vec<ModelChunk>> {
        let mut state = self.state.lock().expect("buffered sink poisoned");
        if let Some(message) = &state.limit_error {
            return Err(buffer_limit_error(message.clone()));
        }
        Ok(std::mem::take(&mut state.chunks))
    }
}

#[async_trait]
impl ModelEventSink for BufferedSink {
    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        let chunk_bytes = buffered_chunk_bytes(&chunk);
        let mut state = self.state.lock().expect("buffered sink poisoned");
        if let Some(message) = &state.limit_error {
            return Err(buffer_limit_error(message.clone()));
        }
        let next_chunks = state.chunks.len().saturating_add(1);
        let next_bytes = state.bytes.saturating_add(chunk_bytes);
        if next_chunks > self.max_chunks || next_bytes > self.max_bytes {
            let message = format!(
                "buffered model stream exceeded its limits (chunks {next_chunks}/{}, bytes {next_bytes}/{})",
                self.max_chunks, self.max_bytes
            );
            state.limit_error = Some(message.clone());
            return Err(buffer_limit_error(message));
        }
        state.bytes = next_bytes;
        state.chunks.push(chunk);
        Ok(())
    }
}

fn buffered_chunk_bytes(chunk: &ModelChunk) -> usize {
    let inline = std::mem::size_of::<ModelChunk>();
    let dynamic = match chunk {
        ModelChunk::TextDelta { delta } => delta.capacity(),
        ModelChunk::ToolCallDelta {
            call_id,
            name,
            arguments_delta,
        } => call_id
            .capacity()
            .saturating_add(name.as_ref().map_or(0, String::capacity))
            .saturating_add(arguments_delta.capacity()),
        ModelChunk::Retrying { .. } | ModelChunk::Done => 0,
    };
    inline.saturating_add(dynamic)
}

fn buffer_limit_error(message: String) -> AgentError {
    AgentError::ModelProtocol {
        kind: ModelProtocolErrorKind::MalformedEvent,
        message,
    }
}

fn stamp_attempt_usage(output: &mut ModelOutput, attempts: u32) {
    output.usage.attempts = attempts.max(1);
    output.usage.retries = output.usage.attempts.saturating_sub(1);
}

async fn retry<T, F, Fut>(
    max_attempts: u32,
    schedule: &RetrySchedule,
    cancel: &CancellationToken,
    mut op: F,
) -> AgentResult<(T, u32)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AgentResult<T>>,
{
    let mut budget = RetryBudget::new(max_attempts);
    loop {
        match op().await {
            Ok(value) => return Ok((value, budget.attempts)),
            Err(error) => {
                let Some(class) = retry_class(&error) else {
                    return Err(error);
                };
                let Some(reservation) = budget.reserve_retry(class) else {
                    return Err(error);
                };
                let delay = retry_delay(class, schedule, reservation.class_retry_number, &error);
                log_retry(
                    reservation.next_attempt,
                    reservation.total_attempt_limit,
                    delay.as_millis().min(u64::MAX as u128) as u64,
                    &error,
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Err(AgentError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ModelOutput, ModelProtocolErrorKind, ModelUsage};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

    #[test]
    fn retry_reason_label_names_the_retried_kind() {
        assert_eq!(
            retry_reason_label(&AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                message: "arguments ended early".into(),
            }),
            "malformed tool-call JSON"
        );
        assert_eq!(
            retry_reason_label(&AgentError::Transport {
                retryable: true,
                message: "connection reset".into(),
            }),
            "retryable transport error"
        );
        assert_eq!(
            retry_reason_label(&AgentError::InvalidRequest("boot".into())),
            "unexpected retryable error"
        );
    }

    fn request() -> ModelRequest {
        ModelRequest {
            messages: Vec::new(),
            tools: Vec::new(),
            metadata: json!({}),
            cancel: CancellationToken::new(),
        }
    }

    /// A recording sink, so tests can assert exactly what reached the
    /// listener (and that a retry never duplicates it).
    #[derive(Debug, Default)]
    struct RecordingSink {
        chunks: std::sync::Mutex<Vec<ModelChunk>>,
    }

    #[async_trait]
    impl ModelEventSink for RecordingSink {
        async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
            self.chunks.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    /// Simulates a custom sink that publishes before discovering its own
    /// retryable delivery failure. Replay must still fail closed.
    #[derive(Debug, Default)]
    struct PublishesThenFailsSink {
        chunks: std::sync::Mutex<Vec<ModelChunk>>,
    }

    #[async_trait]
    impl ModelEventSink for PublishesThenFailsSink {
        async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
            self.chunks.lock().unwrap().push(chunk);
            Err(AgentError::Transport {
                retryable: true,
                message: "sink failed after publish".into(),
            })
        }
    }

    /// Mirrors the product sink: tool-call deltas are consumed internally and
    /// only text creates an externally visible replay boundary.
    #[derive(Debug, Default)]
    struct InternalToolDeltaSink {
        chunks: std::sync::Mutex<Vec<ModelChunk>>,
    }

    #[async_trait]
    impl ModelEventSink for InternalToolDeltaSink {
        fn creates_replay_barrier(&self, chunk: &ModelChunk) -> bool {
            matches!(chunk, ModelChunk::TextDelta { .. })
        }

        async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
            self.chunks.lock().unwrap().push(chunk);
            Ok(())
        }
    }

    struct Flaky {
        calls: Arc<AtomicU32>,
        failures_before_success: u32,
        retryable: bool,
    }

    #[async_trait]
    impl ModelTransport for Flaky {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.failures_before_success {
                return Err(AgentError::Transport {
                    retryable: self.retryable,
                    message: "boom".into(),
                });
            }
            Ok(ModelOutput {
                content: "ok".into(),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn retries_retryable_errors_with_backoff() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = Flaky {
            calls: calls.clone(),
            failures_before_success: 2,
            retryable: true,
        };
        let transport = RetryingTransport::new(inner, 5, Duration::from_millis(1));
        let output = transport.complete(request()).await.unwrap();
        assert_eq!(output.content, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3, "2 failures then success");
        assert_eq!(output.usage.attempts, 3);
        assert_eq!(output.usage.retries, 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = Flaky {
            calls: calls.clone(),
            failures_before_success: 100,
            retryable: true,
        };
        let transport = RetryingTransport::new(inner, 3, Duration::from_millis(1));
        let error = transport.complete(request()).await.unwrap_err();
        assert!(matches!(
            error,
            AgentError::Transport {
                retryable: true,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_errors() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = Flaky {
            calls: calls.clone(),
            failures_before_success: 100,
            retryable: false,
        };
        let transport = RetryingTransport::new(inner, 5, Duration::from_millis(1));
        let error = transport.complete(request()).await.unwrap_err();
        assert!(matches!(
            error,
            AgentError::Transport {
                retryable: false,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Fails with a transport error *after* emitting one delta on the first
    /// call; succeeds on the second.
    struct EmitsThenFails {
        calls: Arc<AtomicU32>,
        retryable: bool,
    }

    #[async_trait]
    impl ModelTransport for EmitsThenFails {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            unreachable!("streaming model should be driven through complete_stream")
        }
        async fn complete_stream(
            &self,
            _request: ModelRequest,
            sink: &dyn ModelEventSink,
        ) -> AgentResult<ModelOutput> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                sink.on_chunk(ModelChunk::TextDelta {
                    delta: "partial".into(),
                })
                .await?;
                return Err(AgentError::Transport {
                    retryable: self.retryable,
                    message: "stream broke".into(),
                });
            }
            sink.on_chunk(ModelChunk::Done).await?;
            Ok(ModelOutput {
                content: "full".into(),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
            })
        }
    }

    /// Always emits one delta, then fails with a retryable transport error.
    struct AlwaysFailingStream {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ModelTransport for AlwaysFailingStream {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            unreachable!("streaming model should be driven through complete_stream")
        }
        async fn complete_stream(
            &self,
            _request: ModelRequest,
            sink: &dyn ModelEventSink,
        ) -> AgentResult<ModelOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            sink.on_chunk(ModelChunk::TextDelta {
                delta: "partial".into(),
            })
            .await?;
            Err(AgentError::Transport {
                retryable: true,
                message: "connection reset".into(),
            })
        }
    }

    #[tokio::test]
    async fn stream_that_failed_after_emitting_is_not_retried() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenFails {
            calls: calls.clone(),
            retryable: true,
        };
        let transport = RetryingTransport::new(inner, 5, Duration::from_millis(1));
        let sink = RecordingSink::default();

        let error = transport
            .complete_stream(request(), &sink)
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AgentError::Transport {
                    retryable: true,
                    ..
                }
            ),
            "the stream failure must surface, got: {error}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a stream that already emitted must not be replayed"
        );
        let chunks = sink.chunks.lock().unwrap();
        assert_eq!(
            &chunks[..],
            &[ModelChunk::TextDelta {
                delta: "partial".into()
            }],
            "the listener must see exactly the first attempt's output"
        );
    }

    /// Fails with a retryable error before emitting anything; succeeds on the
    /// second call — this is the retryable shape.
    struct FlakyStream {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ModelTransport for FlakyStream {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            unreachable!("streaming model should be driven through complete_stream")
        }
        async fn complete_stream(
            &self,
            _request: ModelRequest,
            sink: &dyn ModelEventSink,
        ) -> AgentResult<ModelOutput> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                return Err(AgentError::Transport {
                    retryable: true,
                    message: "connection reset".into(),
                });
            }
            sink.on_chunk(ModelChunk::TextDelta {
                delta: "hello".into(),
            })
            .await?;
            sink.on_chunk(ModelChunk::Done).await?;
            Ok(ModelOutput {
                content: "hello".into(),
                tool_calls: Vec::new(),
                usage: ModelUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn stream_failure_before_any_delta_is_retried() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = FlakyStream {
            calls: calls.clone(),
        };
        let transport = RetryingTransport::new(inner, 5, Duration::from_millis(1))
            .with_jitter(|delay, _| delay);
        let sink = RecordingSink::default();

        let output = transport.complete_stream(request(), &sink).await.unwrap();
        assert_eq!(output.content, "hello");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(output.usage.attempts, 2);
        assert_eq!(output.usage.retries, 1);
        let chunks = sink.chunks.lock().unwrap();
        assert_eq!(
            &chunks[..],
            &[
                ModelChunk::Retrying {
                    attempt: 2,
                    delay_ms: 1,
                },
                ModelChunk::TextDelta {
                    delta: "hello".into()
                },
                ModelChunk::Done,
            ],
            "the listener sees retry progress and exactly one stream's output"
        );
    }

    /// Buffering mode exists for outcome-measuring harnesses: a mid-stream
    /// retryable failure replays from scratch and only the successful
    /// attempt reaches the real sink.
    #[tokio::test]
    async fn buffering_mode_replays_after_midstream_failure() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenFails {
            calls: calls.clone(),
            retryable: true,
        };
        let transport = RetryingTransport::new_buffering(inner, 5, Duration::from_millis(1))
            .with_jitter(|delay, _| delay);
        let sink = RecordingSink::default();

        let output = transport.complete_stream(request(), &sink).await.unwrap();
        assert_eq!(output.content, "full");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one replay after the break"
        );
        assert_eq!(output.usage.attempts, 2);
        assert_eq!(output.usage.retries, 1);
        let chunks = sink.chunks.lock().unwrap();
        assert_eq!(
            &chunks[..],
            &[
                ModelChunk::Retrying {
                    attempt: 2,
                    delay_ms: 1,
                },
                ModelChunk::Done,
            ],
            "the listener sees retry progress and only the successful attempt"
        );
    }

    #[tokio::test]
    async fn buffering_mode_surfaces_non_retryable_immediately() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenFails {
            calls: calls.clone(),
            retryable: false,
        };
        let transport = RetryingTransport::new_buffering(inner, 5, Duration::from_millis(1));
        let sink = RecordingSink::default();

        let error = transport
            .complete_stream(request(), &sink)
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AgentError::Transport {
                    retryable: false,
                    ..
                }
            ),
            "the non-retryable failure must surface, got: {error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            sink.chunks.lock().unwrap().is_empty(),
            "buffered output must not leak to the listener on failure"
        );
    }

    /// Fails the first attempt with malformed tool-call argument JSON (model
    /// output noise) after emitting one delta; succeeds on the second.
    struct EmitsThenMalformedCall {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl ModelTransport for EmitsThenMalformedCall {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            unreachable!("streaming model should be driven through complete_stream")
        }
        async fn complete_stream(
            &self,
            _request: ModelRequest,
            sink: &dyn ModelEventSink,
        ) -> AgentResult<ModelOutput> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                sink.on_chunk(ModelChunk::ToolCallDelta {
                    call_id: "call-1".into(),
                    name: Some("fs_read".into()),
                    arguments_delta: "{\"path\": \"src".into(),
                })
                .await?;
                return Err(AgentError::ModelProtocol {
                    kind: ModelProtocolErrorKind::MalformedToolCall,
                    message: "EOF while parsing a list at line 1 column 10526".into(),
                });
            }
            sink.on_chunk(ModelChunk::Done).await?;
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![agent_contracts::ToolCall {
                    id: "call-1".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "src/lib.rs"}),
                }],
                usage: ModelUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn buffering_mode_retries_malformed_tool_call_arguments_from_scratch() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenMalformedCall {
            calls: calls.clone(),
        };
        let transport = RetryingTransport::new_buffering(inner, 3, Duration::from_millis(1))
            .with_jitter(|delay, _| delay);
        let sink = RecordingSink::default();

        let output = transport.complete_stream(request(), &sink).await.unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].name, "fs.read");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one replay after the malformed call"
        );
        assert_eq!(output.usage.attempts, 2);
        assert_eq!(output.usage.retries, 1);
        let chunks = sink.chunks.lock().unwrap();
        assert!(
            !chunks
                .iter()
                .any(|chunk| matches!(chunk, ModelChunk::ToolCallDelta { .. })),
            "the rejected attempt's deltas must not reach the sink"
        );
        assert!(chunks.contains(&ModelChunk::Done));
    }

    #[tokio::test]
    async fn live_mode_retries_malformed_internal_tool_deltas_before_publication() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenMalformedCall {
            calls: calls.clone(),
        };
        let transport = RetryingTransport::new(inner, 3, Duration::from_millis(1))
            .with_jitter(|delay, _| delay);
        let sink = InternalToolDeltaSink::default();

        let output = transport.complete_stream(request(), &sink).await.unwrap();

        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.usage.attempts, 2);
        assert_eq!(output.usage.retries, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let chunks = sink.chunks.lock().unwrap();
        assert!(chunks.iter().any(|chunk| matches!(
            chunk,
            ModelChunk::Retrying {
                attempt: 2,
                delay_ms: 0
            }
        )));
        assert!(chunks.contains(&ModelChunk::Done));
    }

    #[tokio::test]
    async fn live_mode_does_not_replay_after_malformed_tool_call_deltas() {
        // Interactive hosts stream deltas immediately; once the malformed
        // delta reached the sink it cannot be un-sent, and a retry would
        // duplicate output, so the error surfaces instead.
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenMalformedCall {
            calls: calls.clone(),
        };
        let transport = RetryingTransport::new(inner, 3, Duration::from_millis(1));
        let sink = RecordingSink::default();

        let error = transport
            .complete_stream(request(), &sink)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                ..
            }
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a live stream that already emitted must not be replayed"
        );
        let chunks = sink.chunks.lock().unwrap();
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| matches!(chunk, ModelChunk::ToolCallDelta { .. }))
                .count(),
            1,
            "the listener sees exactly the first attempt's truncated delta"
        );
    }

    #[tokio::test]
    async fn sink_that_fails_after_publication_cannot_be_replayed() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = EmitsThenFails {
            calls: calls.clone(),
            retryable: true,
        };
        let transport = RetryingTransport::new(inner, 3, Duration::from_millis(1));
        let sink = PublishesThenFailsSink::default();

        let error = transport
            .complete_stream(request(), &sink)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::Transport {
                retryable: true,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(sink.chunks.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn buffering_mode_gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = AlwaysFailingStream {
            calls: calls.clone(),
        };
        let transport = RetryingTransport::new_buffering(inner, 3, Duration::from_millis(1))
            .with_jitter(|delay, _| delay);
        let sink = RecordingSink::default();

        let error = transport
            .complete_stream(request(), &sink)
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AgentError::Transport {
                    retryable: true,
                    ..
                }
            ),
            "the last retryable failure must surface, got: {error}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            &sink.chunks.lock().unwrap()[..],
            &[
                ModelChunk::Retrying {
                    attempt: 2,
                    delay_ms: 1,
                },
                ModelChunk::Retrying {
                    attempt: 3,
                    delay_ms: 2,
                },
            ],
            "retry progress stays observable even when all attempts fail"
        );
    }

    #[tokio::test]
    async fn cancelled_backoff_aborts_immediately() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = Flaky {
            calls: calls.clone(),
            failures_before_success: 100,
            retryable: true,
        };
        let transport = RetryingTransport::new(inner, 5, Duration::from_secs(60));
        let token = CancellationToken::new();
        let request = ModelRequest {
            messages: Vec::new(),
            tools: Vec::new(),
            metadata: json!({}),
            cancel: token.clone(),
        };
        let run = tokio::spawn(async move { transport.complete(request).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .expect("cancellation must abort the backoff")
            .expect("retry task panicked");
        assert!(
            matches!(result, Err(AgentError::Cancelled)),
            "a cancelled request must not sleep out the backoff, got: {result:?}"
        );
    }

    #[test]
    fn backoff_is_checked_capped_and_deterministically_jitterable() {
        let mut schedule = RetrySchedule::new(Duration::from_secs(2));
        schedule.max_delay = Duration::from_secs(7);
        schedule.jitter = Arc::new(|delay, _| delay / 2);
        let error = AgentError::Transport {
            retryable: true,
            message: "transient".into(),
        };

        assert_eq!(schedule.delay(2, &error), Duration::from_secs(2));
        assert_eq!(
            schedule.delay(u32::MAX, &error),
            Duration::from_millis(3_500),
            "a huge exponent saturates at the cap before jitter instead of overflowing"
        );
    }

    #[test]
    fn retry_after_is_a_bounded_floor_for_the_schedule() {
        let mut schedule = RetrySchedule::new(Duration::from_millis(10));
        schedule.max_delay = Duration::from_secs(5);
        schedule.jitter = Arc::new(|delay, _| delay);
        let error = AgentError::TransportRetryAfter {
            retry_after_ms: RetryAfterMillis::new_saturating(50_000),
            message: "rate limited".into(),
        };

        assert_eq!(schedule.delay(1, &error), Duration::from_secs(5));
        assert!(retryable(&error));
    }

    #[test]
    fn protocol_damage_is_never_retried() {
        let error = AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: "damaged SSE JSON".into(),
        };
        assert!(!retryable(&error));
    }

    #[test]
    fn malformed_model_tool_call_arguments_are_retryable() {
        let malformed_call = AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedToolCall,
            message: "EOF while parsing a list at line 1 column 10526".into(),
        };
        let wire_damage = AgentError::ModelProtocol {
            kind: ModelProtocolErrorKind::MalformedEvent,
            message: "damaged SSE JSON".into(),
        };
        assert!(
            retryable(&malformed_call),
            "model-emitted malformed tool arguments are transient output the model can fix"
        );
        assert!(
            !retryable(&wire_damage),
            "wire/protocol damage stays non-retryable"
        );
        let class = retry_class(&malformed_call).expect("format class");
        assert_eq!(class, RetryClass::ToolCallFormat);
        assert_eq!(retry_attempt_limit(class, 6), 2);
        assert_eq!(
            retry_delay(
                class,
                &RetrySchedule::new(Duration::from_secs(2)),
                1,
                &malformed_call,
            ),
            Duration::ZERO,
            "format regeneration does not use outage backoff"
        );
    }

    #[test]
    fn transport_and_format_retries_have_independent_bounded_credits() {
        let mut budget = RetryBudget::new(3);
        assert_eq!(
            budget.reserve_retry(RetryClass::ToolCallFormat),
            Some(RetryReservation {
                next_attempt: 2,
                total_attempt_limit: 4,
                class_retry_number: 1,
            })
        );
        assert_eq!(
            budget.reserve_retry(RetryClass::Transport),
            Some(RetryReservation {
                next_attempt: 3,
                total_attempt_limit: 4,
                class_retry_number: 1,
            })
        );
        assert_eq!(
            budget.reserve_retry(RetryClass::Transport),
            Some(RetryReservation {
                next_attempt: 4,
                total_attempt_limit: 4,
                class_retry_number: 2,
            })
        );
        assert_eq!(budget.reserve_retry(RetryClass::Transport), None);

        let mut reverse = RetryBudget::new(3);
        assert!(reverse.reserve_retry(RetryClass::Transport).is_some());
        assert!(reverse.reserve_retry(RetryClass::ToolCallFormat).is_some());
        assert_eq!(reverse.format_failures, 1);
        assert_eq!(reverse.transport_failures, 1);
    }

    #[tokio::test]
    async fn persistent_malformed_arguments_spend_only_one_regeneration() {
        let calls = Arc::new(AtomicU32::new(0));
        let schedule = RetrySchedule::new(Duration::from_secs(2));
        let cancel = CancellationToken::new();

        let error = retry(6, &schedule, &cancel, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err::<(), _>(AgentError::ModelProtocol {
                    kind: ModelProtocolErrorKind::MalformedToolCall,
                    message: "EOF while parsing tool arguments".into(),
                })
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn buffered_sink_enforces_a_sticky_chunk_limit() {
        let sink = BufferedSink::with_limits(1, usize::MAX);
        sink.on_chunk(ModelChunk::Done).await.unwrap();
        let error = sink.on_chunk(ModelChunk::Done).await.unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
        assert!(
            sink.take().is_err(),
            "a transport that ignores the sink refusal cannot publish the bounded prefix"
        );
    }

    #[tokio::test]
    async fn buffered_sink_enforces_a_resident_byte_limit() {
        let sink = BufferedSink::with_limits(8, std::mem::size_of::<ModelChunk>() + 8);
        let error = sink
            .on_chunk(ModelChunk::TextDelta {
                delta: "x".repeat(64),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedEvent,
                ..
            }
        ));
        assert!(error.to_string().contains("bytes"));
    }

    #[tokio::test]
    async fn cancellation_token_wakes_waiters() {
        let token = CancellationToken::new();
        let waiter = token.clone();
        let handle = tokio::spawn(async move {
            waiter.cancelled().await;
        });
        // Give the waiter a chance to register.
        tokio::task::yield_now().await;
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("waiter did not wake")
            .expect("waiter panicked");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn already_cancelled_resolves_immediately() {
        let token = CancellationToken::new();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), token.cancelled())
            .await
            .expect("cancelled() on an already-cancelled token must resolve");
    }
}
