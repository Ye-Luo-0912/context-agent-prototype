//! Live event sink: forwards streamed model deltas to subscribers without
//! journaling them (the final `AssistantMessage` carries the complete content
//! for replay).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agent_contracts::{
    AgentResult, ModelChunk, ModelEventSink, RunId, RuntimeEvent, RuntimeEventEnvelope,
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub(crate) struct LiveSink {
    event_tx: broadcast::Sender<RuntimeEventEnvelope>,
    seq: Arc<AtomicU64>,
    run_id: RunId,
}

impl LiveSink {
    pub(crate) fn new(
        event_tx: broadcast::Sender<RuntimeEventEnvelope>,
        seq: Arc<AtomicU64>,
        run_id: RunId,
    ) -> Self {
        Self {
            event_tx,
            seq,
            run_id,
        }
    }

    fn emit_live(&self, event: RuntimeEvent) {
        let envelope = RuntimeEventEnvelope {
            run_id: self.run_id,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_ms: now_ms(),
            event,
        };
        let _ = self.event_tx.send(envelope);
    }
}

#[async_trait::async_trait]
impl ModelEventSink for LiveSink {
    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        match chunk {
            ModelChunk::TextDelta { delta } => {
                self.emit_live(RuntimeEvent::ModelDelta { delta });
                Ok(())
            }
            // Tool-call argument deltas are internal to the model round; the
            // runtime surfaces tool execution via ToolStarted/ToolFinished.
            ModelChunk::ToolCallDelta { .. } | ModelChunk::Done => Ok(()),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}
