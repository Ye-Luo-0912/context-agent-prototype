//! Generic retry/backoff wrapper for any `ModelTransport`.
//!
//! Only errors marked retryable (`AgentError::Transport { retryable: true }`,
//! i.e. network failures, timeouts, 5xx, 429, and gateway-wrapped upstream
//! 400 `Upstream request failed`) are retried. Auth errors and genuine
//! provider-level rejections fail immediately. The backoff yields to the
//! request's cancellation token, so a cancelled request aborts instead of
//! sleeping out the wait.
//!
//! Streaming is retryable only while nothing has reached the sink: a stream
//! that already emitted deltas cannot be replayed into the same sink, and a
//! retry would duplicate the live output. The wrapper tracks emission and
//! surfaces the error instead of retrying in that case.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ModelCapabilities, ModelChunk, ModelEventSink,
    ModelOutput, ModelRequest, ModelTransport,
};
use async_trait::async_trait;

pub struct RetryingTransport<T: ModelTransport> {
    inner: T,
    max_attempts: u32,
    base_delay: Duration,
    /// Whole-response buffering mode: chunks are collected internally and
    /// only forwarded to the real sink after a successful attempt, so a
    /// retryable mid-stream failure can replay from scratch. Harnesses
    /// measure outcomes rather than render live deltas; interactive hosts
    /// keep the live mode where already-emitted output blocks a replay.
    buffering: bool,
}

impl<T: ModelTransport> RetryingTransport<T> {
    pub fn new(inner: T, max_attempts: u32, base_delay: Duration) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            base_delay,
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
            base_delay,
            buffering: true,
        }
    }
}

/// Forwards chunks and records whether anything reached the sink, so the
/// retry wrapper can refuse to replay a stream that already produced output.
struct EmissionTrackingSink<'a> {
    inner: &'a dyn ModelEventSink,
    emitted: &'a AtomicBool,
}

#[async_trait]
impl ModelEventSink for EmissionTrackingSink<'_> {
    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        self.emitted.store(true, Ordering::Relaxed);
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
            retry(self.max_attempts, self.base_delay, &request.cancel, || {
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
    /// Live mode: forward chunks as they arrive; a retryable failure after
    /// anything was emitted cannot be replayed into the same sink, so it is
    /// surfaced instead of retried.
    async fn complete_stream_live(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        let emitted = AtomicBool::new(false);
        let mut attempt = 0u32;
        loop {
            let tracking = EmissionTrackingSink {
                inner: sink,
                emitted: &emitted,
            };
            match self.inner.complete_stream(request.clone(), &tracking).await {
                Ok(mut output) => {
                    stamp_attempt_usage(&mut output, attempt + 1);
                    return Ok(output);
                }
                Err(error) => {
                    let retryable = matches!(
                        &error,
                        AgentError::Transport {
                            retryable: true,
                            ..
                        }
                    );
                    if attempt + 1 >= self.max_attempts
                        || !retryable
                        || emitted.load(Ordering::Relaxed)
                    {
                        // A stream that already emitted deltas cannot be
                        // replayed into the same sink: the live listener has
                        // no rewind, and a retry would duplicate the output.
                        return Err(error);
                    }
                    attempt += 1;
                    let delay = self.base_delay * 2u32.pow(attempt.saturating_sub(1));
                    sink.on_chunk(ModelChunk::Retrying {
                        attempt: attempt + 1,
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
        let mut attempt = 0u32;
        loop {
            let collected = BufferedSink::default();
            match self
                .inner
                .complete_stream(request.clone(), &collected)
                .await
            {
                Ok(mut output) => {
                    for chunk in collected.take() {
                        sink.on_chunk(chunk).await?;
                    }
                    stamp_attempt_usage(&mut output, attempt + 1);
                    return Ok(output);
                }
                Err(error) => {
                    let retryable = matches!(
                        &error,
                        AgentError::Transport {
                            retryable: true,
                            ..
                        }
                    );
                    if attempt + 1 >= self.max_attempts || !retryable {
                        return Err(error);
                    }
                    attempt += 1;
                    let delay = self.base_delay * 2u32.pow(attempt.saturating_sub(1));
                    sink.on_chunk(ModelChunk::Retrying {
                        attempt: attempt + 1,
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

/// Collects one attempt's chunks for the buffering mode. Chunks are bounded
/// upstream by the provider's byte cap, so this stays memory-bounded.
#[derive(Default)]
struct BufferedSink {
    chunks: std::sync::Mutex<Vec<ModelChunk>>,
}

impl BufferedSink {
    fn take(&self) -> Vec<ModelChunk> {
        std::mem::take(&mut *self.chunks.lock().expect("buffered sink poisoned"))
    }
}

#[async_trait]
impl ModelEventSink for BufferedSink {
    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        self.chunks
            .lock()
            .expect("buffered sink poisoned")
            .push(chunk);
        Ok(())
    }
}

fn stamp_attempt_usage(output: &mut ModelOutput, attempts: u32) {
    output.usage.attempts = attempts.max(1);
    output.usage.retries = output.usage.attempts.saturating_sub(1);
}

async fn retry<T, F, Fut>(
    max_attempts: u32,
    base_delay: Duration,
    cancel: &CancellationToken,
    mut op: F,
) -> AgentResult<(T, u32)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AgentResult<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok((value, attempt + 1)),
            Err(error) => {
                if attempt + 1 >= max_attempts
                    || !matches!(
                        &error,
                        AgentError::Transport {
                            retryable: true,
                            ..
                        }
                    )
                {
                    return Err(error);
                }
                attempt += 1;
                // Exponential backoff with jitter-free doubling: 1x, 2x, 4x, ...
                let delay = base_delay * 2u32.pow(attempt.saturating_sub(1));
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
    use agent_contracts::{ModelOutput, ModelUsage};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

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
        let transport = RetryingTransport::new(inner, 5, Duration::from_millis(1));
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
        let transport = RetryingTransport::new_buffering(inner, 5, Duration::from_millis(1));
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

    #[tokio::test]
    async fn buffering_mode_gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicU32::new(0));
        let inner = AlwaysFailingStream {
            calls: calls.clone(),
        };
        let transport = RetryingTransport::new_buffering(inner, 3, Duration::from_millis(1));
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
