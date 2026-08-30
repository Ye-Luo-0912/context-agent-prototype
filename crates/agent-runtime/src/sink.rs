//! Live event sink: forwards streamed model deltas to subscribers without
//! journaling them (the final `AssistantMessage` carries the complete content
//! for replay).

use std::time::{SystemTime, UNIX_EPOCH};

use agent_contracts::{
    AgentResult, ModelChunk, ModelEventSink, OperationId, RunId, RuntimeEvent,
    RuntimeEventEnvelope, TurnId,
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub(crate) struct LiveSink {
    event_tx: broadcast::Sender<RuntimeEventEnvelope>,
    /// Durable cursor of the `ModelStarted` event that opened this stream.
    /// Live deltas repeat it; they never allocate journal sequence numbers.
    journal_cursor: u64,
    run_id: RunId,
    /// The operation this sink belongs to. Every streamed delta carries this
    /// identity so a consumer can drop deltas from a superseded turn.
    turn_id: TurnId,
    operation_id: OperationId,
    generation: u64,
}

impl LiveSink {
    pub(crate) fn new(
        event_tx: broadcast::Sender<RuntimeEventEnvelope>,
        journal_cursor: u64,
        run_id: RunId,
        turn_id: TurnId,
        operation_id: OperationId,
        generation: u64,
    ) -> Self {
        Self {
            event_tx,
            journal_cursor,
            run_id,
            turn_id,
            operation_id,
            generation,
        }
    }

    fn emit_live(&self, event: RuntimeEvent) {
        let envelope = RuntimeEventEnvelope {
            run_id: self.run_id,
            seq: self.journal_cursor,
            timestamp_ms: now_ms(),
            event,
        };
        let _ = self.event_tx.send(envelope);
    }
}

#[async_trait::async_trait]
impl ModelEventSink for LiveSink {
    fn creates_replay_barrier(&self, chunk: &ModelChunk) -> bool {
        // Tool-call deltas are parsed internally and never reach subscribers;
        // Done carries no content. A malformed tool call can therefore retry
        // safely until a text delta has actually been published.
        matches!(chunk, ModelChunk::TextDelta { .. })
    }

    async fn on_chunk(&self, chunk: ModelChunk) -> AgentResult<()> {
        match chunk {
            ModelChunk::TextDelta { delta } => {
                self.emit_live(RuntimeEvent::ModelDelta {
                    turn_id: self.turn_id,
                    operation_id: self.operation_id,
                    generation: self.generation,
                    delta,
                });
                Ok(())
            }
            ModelChunk::Retrying { attempt, delay_ms } => {
                self.emit_live(RuntimeEvent::ModelRetrying {
                    turn_id: self.turn_id,
                    operation_id: self.operation_id,
                    generation: self.generation,
                    attempt,
                    delay_ms,
                });
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
