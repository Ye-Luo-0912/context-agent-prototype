//! The current task directive's full-body identity. `turn_intent` is only
//! its display preview; continuation must resolve this retained input.

use agent_contracts::{
    AgentError, AgentResult, ArtifactLocator, ContentDigest, InputAuthority, InputKind,
    InputLifecycle, InputSource, MAX_COMPLETION_REF_CHARS, RuntimeInputEnvelope, TaskId,
    USER_INPUT_ARTIFACT_OWNER, USER_INPUT_MAX_BYTES, USER_INPUT_PREVIEW_CHARS, bounded_preview,
};
use agent_workspace::Workspace;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

/// One task-owned reference to its last applied user instruction. Durable
/// compositions retain the existing sealed input artifact, not another copy
/// of the body. Without an artifact workspace the same byte bound applies
/// to the inline fallback. Neither form grants permission to replay effects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDirective {
    pub input: RuntimeInputEnvelope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_body: Option<String>,
}

impl TaskDirective {
    pub(crate) fn capture(
        task_id: TaskId,
        content: &str,
        mut input: RuntimeInputEnvelope,
    ) -> AgentResult<Self> {
        if content.len() > USER_INPUT_MAX_BYTES {
            return Err(invalid("body exceeds the user-input byte limit"));
        }
        let digest = ContentDigest::sha256_bytes(content.as_bytes()).to_string();
        if input
            .digest
            .as_ref()
            .is_some_and(|recorded| recorded != &digest)
        {
            return Err(invalid("body digest differs from the applied input"));
        }
        input.digest = Some(digest);
        let inline_body = input.body_ref.is_none().then(|| content.to_string());
        let directive = Self { input, inline_body };
        directive.validate(task_id)?;
        directive.validate_body(content)?;
        Ok(directive)
    }

    /// Structural validation runs before checkpoint restore mutates any
    /// plane. Artifact availability is checked on continuation so one missing
    /// suspended task's body does not prevent inspecting the other tasks.
    pub(crate) fn validate(&self, task_id: TaskId) -> AgentResult<()> {
        self.input.validate().map_err(invalid)?;
        if self.input.task_id != Some(task_id)
            || self.input.source != InputSource::User
            || self.input.authority != InputAuthority::UserSteering
            || self.input.kind != InputKind::Dialogue
            || self.input.lifecycle != InputLifecycle::Applied
            || self.input.input_id.is_none_or(|id| id.0.is_nil())
        {
            return Err(invalid("must identify this task's applied user dialogue"));
        }
        if self.input.bytes == 0 || self.input.bytes > USER_INPUT_MAX_BYTES as u64 {
            return Err(invalid("body size is outside the user-input byte limit"));
        }
        let digest = self
            .input
            .digest
            .as_deref()
            .ok_or_else(|| invalid("missing body digest"))?;
        let digest: ContentDigest = digest.parse().map_err(|_| invalid("invalid body digest"))?;
        match (&self.input.body_ref, &self.inline_body) {
            (Some(reference), None) => {
                if reference.chars().count() > MAX_COMPLETION_REF_CHARS {
                    return Err(invalid("artifact reference exceeds the reference limit"));
                }
                let locator = ArtifactLocator::parse_sealed(reference)
                    .map_err(|error| invalid(error.to_string()))?;
                if locator.owner() != USER_INPUT_ARTIFACT_OWNER || locator.digest() != Some(digest)
                {
                    return Err(invalid(
                        "artifact must name the same sealed user-input digest",
                    ));
                }
            }
            (None, Some(body)) => self.validate_body(body)?,
            _ => {
                return Err(invalid(
                    "must have exactly one artifact or inline body owner",
                ));
            }
        }
        Ok(())
    }

    fn validate_body(&self, body: &str) -> AgentResult<()> {
        if body.len() > USER_INPUT_MAX_BYTES
            || body.len() as u64 != self.input.bytes
            || Some(ContentDigest::sha256_bytes(body.as_bytes()).to_string()) != self.input.digest
            || bounded_preview(body, USER_INPUT_PREVIEW_CHARS) != self.input.preview
        {
            return Err(invalid(
                "body does not match its recorded size, digest and preview",
            ));
        }
        Ok(())
    }

    pub(crate) async fn resolve(
        &self,
        task_id: TaskId,
        workspace: Option<&Workspace>,
    ) -> AgentResult<String> {
        self.validate(task_id)?;
        let body = match (&self.input.body_ref, &self.inline_body) {
            (Some(reference), None) => {
                let workspace =
                    workspace.ok_or_else(|| invalid("artifact workspace is unavailable"))?;
                let locator = ArtifactLocator::parse_sealed(reference)
                    .map_err(|error| invalid(error.to_string()))?;
                // A formally restored task still owns the original run's
                // input artifact; the new process must not substitute its
                // current run id or re-persist a different instruction.
                let (_, file) = workspace
                    .open_artifact_for_run(reference, locator.run_id())
                    .await
                    .map_err(|error| invalid(format!("cannot read retained input: {error}")))?;
                let mut bytes = Vec::new();
                file.into_tokio()
                    .take(USER_INPUT_MAX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|error| invalid(format!("cannot read retained input: {error}")))?;
                String::from_utf8(bytes).map_err(|_| invalid("retained input is not UTF-8"))?
            }
            (None, Some(body)) => body.clone(),
            _ => return Err(invalid("body owner is unavailable")),
        };
        self.validate_body(&body)?;
        Ok(body)
    }

    pub(crate) fn continuation(&self, task_id: TaskId, body: &str) -> RuntimeInputEnvelope {
        let mut input = RuntimeInputEnvelope::task_continuation(task_id, body);
        input.causal_parent = self.input.input_id;
        input.body_ref = self.input.body_ref.clone();
        input.digest = self.input.digest.clone();
        input
    }
}

fn invalid(detail: impl std::fmt::Display) -> AgentError {
    AgentError::InvalidRequest(format!(
        "cannot continue the retained task directive: {detail}; resend the complete instruction"
    ))
}
