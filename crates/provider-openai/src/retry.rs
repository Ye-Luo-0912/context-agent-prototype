//! Generic retry/backoff wrapper for any `ModelTransport`.
//!
//! Only errors marked retryable (`AgentError::Transport { retryable: true }`,
//! i.e. network failures, timeouts, 5xx, 429) are retried. Auth errors and
//! provider-level rejections fail immediately.

use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ModelCapabilities, ModelEventSink, ModelOutput, ModelRequest,
    ModelTransport,
};
use async_trait::async_trait;

pub struct RetryingTransport<T: ModelTransport> {
    inner: T,
    max_attempts: u32,
    base_delay: Duration,
}

impl<T: ModelTransport> RetryingTransport<T> {
    pub fn new(inner: T, max_attempts: u32, base_delay: Duration) -> Self {
        Self {
            inner,
            max_attempts: max_attempts.max(1),
            base_delay,
        }
    }
}

#[async_trait]
impl<T: ModelTransport> ModelTransport for RetryingTransport<T> {
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        retry(self.max_attempts, self.base_delay, || {
            self.inner.complete(request.clone())
        })
        .await
    }

    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        retry(self.max_attempts, self.base_delay, || {
            self.inner.complete_stream(request.clone(), sink)
        })
        .await
    }
}

async fn retry<T, F, Fut>(max_attempts: u32, base_delay: Duration, mut op: F) -> AgentResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AgentResult<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
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
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ModelOutput, ModelUsage};
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
