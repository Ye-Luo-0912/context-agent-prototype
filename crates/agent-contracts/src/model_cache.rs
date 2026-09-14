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

/// R6: the routing key is the versioned, length-prefixed serialization of
/// the five-field tuple (isolation, workspace, endpoint, task, lane),
/// digested through the existing SHA-256/ContentDigest facility into a
/// fixed-length opaque string. The per-field byte-length prefixes make the
/// encoding injective: no separator text — however hostile the configured
/// components — can alias two different identities, no component is
/// rewritten, trimmed or truncated, and the digest bounds the wire key
/// regardless of component sizes. The encoding version participates in the
/// digest, so a future encoding change is a fresh key space rather than a
/// silent aliasing of the old one (the `rc2-` wire prefix marks it).
const ROUTING_KEY_ENCODING_VERSION: u8 = 2;
/// Wire prefix of the v2 digest encoding: `rc2-` + 64 lowercase hex.
const ROUTING_KEY_ENCODING_PREFIX: &str = "rc2-";

impl PromptCacheRouting {
    /// The stable routing key for one task on one call lane.
    pub fn key_for(&self, task: &str, lane: &str) -> String {
        // Canonical tuple encoding: one version byte, then for each field
        // in fixed order (isolation, workspace, endpoint, task, lane) its
        // UTF-8 byte length as an 8-byte big-endian prefix followed by the
        // field bytes. Prefix-free and unambiguous; the exact byte stream
        // is hashed whole — never truncated, never rebuilt from pieces.
        let mut hasher = Sha256::new();
        hasher.update([ROUTING_KEY_ENCODING_VERSION]);
        for field in [
            self.isolation.as_bytes(),
            self.workspace.as_bytes(),
            self.endpoint.as_bytes(),
            task.as_bytes(),
            lane.as_bytes(),
        ] {
            hasher.update((field.len() as u64).to_be_bytes());
            hasher.update(field);
        }
        format!(
            "{ROUTING_KEY_ENCODING_PREFIX}{}",
            ContentDigest::from_bytes(hasher.finalize().into())
        )
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
