import re

# ---------- turn.rs ----------
p = 'crates/agent-runtime/src/actor/turn.rs'
s = open(p, encoding='utf-8').read()

# 1. hold point: a pending deferred refresh owns the turn edge
old = '''    async fn spawn_next_model_or_end(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        // A model request consumes only a fully terminalized tool batch.'''
assert old in s, "spawn_next_model_or_end"
new = '''    async fn spawn_next_model_or_end(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        // A deferred proof refresh owns the turn edge: the parked proposal
        // resumes over the proof channel, and the turn finalizes from that
        // resume instead of racing it with a new model round.
        if self.state.pending_proof_refresh.is_some() {
            return;
        }
        // A model request consumes only a fully terminalized tool batch.'''
s = s.replace(old, new)

# 2. resume handler: signature + op_tx, terminal/continue tail instead of drop
old = '''    pub(super) async fn on_proof_refresh_completed(
        &mut self,
        resumed: DeferredProofRefresh,
    ) {'''
assert old in s, "resume sig"
new = '''    pub(super) async fn on_proof_refresh_completed(
        &mut self,
        resumed: DeferredProofRefresh,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {'''
s = s.replace(old, new)

old = '''        let DeferredProofRefresh {
            plan,
            proposal,
            outcome,
            ..
        } = resumed;
        self.apply_proof_refresh_outcome(plan, outcome).await;
        let mut scratch = ToolOutput {
            call_id: format!("deferred-complete-{}", now_ms()),
            tool_name: "task.complete".into(),
            ok: false,
            summary: String::new(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({ "deferred_completion": true }),
        };
        self.finalize_completion_proposal(&mut scratch, proposal)
            .await;
        if !scratch.ok {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Warning {
                    message: crate::output::bound_error_message(
                        "deferred completion dropped: its turn finished first; the refreshed proof is recorded, so the next task.complete is admitted directly"
                            .to_string(),
                    ),
                })
                .await;
        }
    }'''
assert old in s, "resume body"
new = '''        let DeferredProofRefresh {
            plan,
            proposal,
            outcome,
            ..
        } = resumed;
        self.apply_proof_refresh_outcome(plan, outcome).await;
        let mut scratch = ToolOutput {
            call_id: format!("deferred-complete-{}", now_ms()),
            tool_name: "task.complete".into(),
            ok: false,
            summary: String::new(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({ "deferred_completion": true }),
        };
        self.finalize_completion_proposal(&mut scratch, proposal)
            .await;
        // The held turn finalizes exactly like the inline tool tail: an
        // accepted completion takes the terminal transaction; a refusal
        // hands the decision back to a model round with the refreshed
        // verification already in context.
        if let Some(summary) = self.terminal_completion_summary() {
            self.finalize_terminal_completion(summary).await;
        } else {
            if !scratch.ok {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Warning {
                        message: crate::output::bound_error_message(format!(
                            "deferred completion refused after proof refresh: {}",
                            scratch.summary
                        )),
                    })
                    .await;
            }
            self.advance_turn(op_tx).await;
        }
    }'''
s = s.replace(old, new)

# 3. verifier panic cannot wedge the held turn: convert panics to a typed error
old = '''        let task = tokio::spawn(async move {
            let outcome = verifier.verify_exact(request).await;'''
assert old in s, "spawn unwrap"
new = '''        let task = tokio::spawn(async move {
            let outcome = match std::panic::AssertUnwindSafe(verifier.verify_exact(request))
                .catch_unwind()
                .await
            {
                Ok(outcome) => outcome,
                Err(panic) => Err(AgentError::Internal(format!(
                    "proof verifier panicked: {panic}"
                ))),
            };'''
s = s.replace(old, new)

# imports for catch_unwind
old = 'use std::collections::HashSet;'
if old in s:
    s = s.replace(old, old + '\nuse std::panic::{AssertUnwindSafe, catch_unwind};')
else:
    m = re.search(r'^use std::[^;]+;', s, re.M)
    print('std import anchor:', s[m.start():m.end()] if m else None)

open(p, 'w', encoding='utf-8', newline='').write(s)
print("turn.rs holding edits applied")
