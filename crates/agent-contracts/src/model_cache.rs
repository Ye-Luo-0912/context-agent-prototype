//! A request-local reuse hint, never an evidence, permission or cache-hit fact.
//! Only the final packed request may bind it. Adapters must revalidate it
//! against the actual messages and tools before mapping vendor parameters.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ContentDigest, ModelRequest, ModelRole};

const METADATA_KEY: &str = "prompt_reuse_boundary";

/// One complete-message boundary before the changing turn/current state.
/// The digest binds all preceding messages (including roles) and tool schemas.
/// It is not a provider token/cache key and says nothing about cache residency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptReuseBoundary {
    version: u8,
    message_count: usize,
    prefix_digest: ContentDigest,
}

impl PromptReuseBoundary {
    pub fn message_count(&self) -> usize {
        self.message_count
    }

    pub fn prefix_digest(&self) -> ContentDigest {
        self.prefix_digest
    }

    fn derive(request: &ModelRequest, message_count: usize) -> Option<Self> {
        let prefix = request.messages.get(..message_count)?;
        if prefix.last()?.content.is_empty()
            || prefix.iter().any(|message| {
                !matches!(message.role, ModelRole::System | ModelRole::User)
                    || !message.tool_calls.is_empty()
                    || message.tool_call_id.is_some()
            })
        {
            return None;
        }
        // Stream into a constant-size digest; do not allocate another copy
        // of the complete prompt or retain previous requests.
        let mut writer = DigestWriter(Sha256::new());
        serde_json::to_writer(&mut writer, &(prefix, &request.tools)).ok()?;
        Some(Self {
            version: 1,
            message_count,
            prefix_digest: ContentDigest::from_bytes(writer.0.finalize().into()),
        })
    }
}

impl ModelRequest {
    /// Read a typed hint from the existing application metadata envelope.
    /// Legacy requests have none. Malformed, out-of-range or stale hints
    /// are ignored; the full request must still be sent unchanged.
    pub fn prompt_reuse_boundary(&self) -> Option<PromptReuseBoundary> {
        let value = self.metadata.get(METADATA_KEY)?;
        let boundary: PromptReuseBoundary = serde_json::from_value(value.clone()).ok()?;
        (boundary.version == 1
            && PromptReuseBoundary::derive(self, boundary.message_count).as_ref()
                == Some(&boundary))
        .then_some(boundary)
    }

    pub(crate) fn bind_prompt_reuse_boundary(&mut self, message_count: usize) {
        if self.metadata.is_null() {
            self.metadata = serde_json::json!({});
        }
        let Some(metadata) = self.metadata.as_object_mut() else {
            return;
        };
        metadata.remove(METADATA_KEY);
        if let Some(boundary) = PromptReuseBoundary::derive(self, message_count) {
            self.metadata[METADATA_KEY] =
                serde_json::to_value(boundary).expect("fixed reuse boundary is serializable");
        }
    }
}

/// N04: the composition root's STABLE cache-routing namespace — who owns
/// the endpoint/workspace pair a task runs in. The key derived from it is
/// an opaque routing string: stable across the rounds and restores of one
/// task, different across isolation domains, never derived from request
/// content (no per-round UUID, no prompt digest). Components come from the
/// composition root because it — not a model alias or a compatible URL —
/// knows which endpoint/workspace it actually configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptCacheRouting {
    /// Deployment-level isolation namespace (opaque to the provider).
    pub isolation: String,
    /// The workspace this task runs against (canonical root or its digest).
    pub workspace: String,
    /// The serving endpoint identity (base URL or the profile digest).
    pub endpoint: String,
}

/// Component bounds keep the derived key a bounded, opaque routing string
/// on the wire — a hostile configuration cannot turn it into an unbounded
/// header.
const ROUTING_COMPONENT_MAX_CHARS: usize = 256;
const ROUTING_KEY_MAX_CHARS: usize = 1024;

impl PromptCacheRouting {
    fn bounded(component: &str) -> String {
        let trimmed = component.trim();
        if trimmed.chars().count() <= ROUTING_COMPONENT_MAX_CHARS {
            trimmed.to_string()
        } else {
            // Oversized components are digested, not truncated: a cut
            // string could collide with a different real component's
            // prefix.
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(trimmed.as_bytes());
            format!("sha256:{:x}", digest)[..64].to_string()
        }
    }

    /// The stable routing key for one task on one call lane.
    pub fn key_for(&self, task: &str, lane: &str) -> String {
        let key = format!(
            "{}|{}|{}|{}|{}",
            Self::bounded(&self.isolation),
            Self::bounded(&self.workspace),
            Self::bounded(&self.endpoint),
            Self::bounded(task),
            Self::bounded(lane),
        );
        if key.chars().count() <= ROUTING_KEY_MAX_CHARS {
            key
        } else {
            key.chars().take(ROUTING_KEY_MAX_CHARS).collect()
        }
    }
}

struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
