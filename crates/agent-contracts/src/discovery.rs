//! Bounded, versioned federated discovery (`CTX-DISC-01..03`, `TOOLS-10`).
//!
//! Search is read-only: a hit is a descriptor, never admission, loading,
//! invocation, or a TaskAnchor mutation. Context and Tool are the only
//! providers in this prototype; there is no public `runtime.search` schema.
//! `context.manage` / `capability.manage` keep their public contracts and
//! share this planner, the descriptor card, and the round caps.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

use crate::{ContextItemId, ExternalizedContext, ToolCatalogEntry};

/// Wire version of `resource://v1/...` refs.
pub const RESOURCE_REF_VERSION: u32 = 1;

/// Max kinds one federated round may fan out to (Context + Tool).
pub const DISCOVERY_MAX_FANOUT: usize = 2;
/// Hard row cap after merge, before paging the public manage surfaces.
pub const DISCOVERY_MAX_ROWS: usize = 32;
/// Model-facing character cap for one discovery page.
pub const DISCOVERY_MAX_RESULT_CHARS: usize = 4_000;
/// Free-text query character cap (same bound as `context.search`).
pub const DISCOVERY_MAX_QUERY_CHARS: usize = 256;
/// Max discovery *search* calls the actor admits in one user turn.
pub const DISCOVERY_MAX_QUERIES_PER_TURN: u32 = 8;
/// Identical search fingerprint budget per user turn.
pub const DISCOVERY_IDENTICAL_QUERY_BUDGET: u32 = 2;
pub const RESOURCE_TITLE_MAX_CHARS: usize = 96;
pub const RESOURCE_SUMMARY_MAX_CHARS: usize = 240;

/// Implemented discovery providers. Other resource kinds stay off this
/// enum until their provider exists — adding a variant is not a search
/// implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Context,
    Tool,
}

impl ResourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Tool => "tool",
        }
    }
}

/// Versioned, typed resource identity. Local transport identity is never a
/// grant; this ref only names what a provider already owns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceRef {
    pub version: u32,
    pub kind: ResourceKind,
    /// Context item id (uuid) or tool name.
    pub id: String,
    /// Provider revision when known (`gc_epoch` / catalog generation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl ResourceRef {
    pub fn context(id: ContextItemId, revision: Option<u64>) -> Self {
        Self {
            version: RESOURCE_REF_VERSION,
            kind: ResourceKind::Context,
            id: id.to_string(),
            revision: revision.map(|epoch| epoch.to_string()),
        }
    }

    pub fn tool(name: impl Into<String>, revision: Option<u64>) -> Self {
        Self {
            version: RESOURCE_REF_VERSION,
            kind: ResourceKind::Tool,
            id: name.into(),
            revision: revision.map(|epoch| epoch.to_string()),
        }
    }

    /// `resource://v1/<kind>/<id>` plus optional `@revision`.
    pub fn uri(&self) -> String {
        let base = format!(
            "resource://v{}/{}/{}",
            self.version,
            self.kind.as_str(),
            self.id
        );
        match &self.revision {
            Some(revision) if !revision.is_empty() => format!("{base}@{revision}"),
            _ => base,
        }
    }

    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("resource://v")?;
        let (version_str, rest) = rest.split_once('/')?;
        let version: u32 = version_str.parse().ok()?;
        let (kind_str, rest) = rest.split_once('/')?;
        let kind = match kind_str {
            "context" => ResourceKind::Context,
            "tool" => ResourceKind::Tool,
            _ => return None,
        };
        let (id, revision) = match rest.split_once('@') {
            Some((id, revision)) if !id.is_empty() => (id.to_string(), Some(revision.to_string())),
            None if !rest.is_empty() => (rest.to_string(), None),
            _ => return None,
        };
        Some(Self {
            version,
            kind,
            id,
            revision,
        })
    }
}

/// Bounded descriptor card. Search returns these; inspect/resolve may
/// enrich them; admit/load/invoke stay explicit follow-up operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    pub r#ref: ResourceRef,
    pub title: String,
    pub summary: String,
    pub owner: String,
    pub lifecycle: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_needs: Vec<String>,
    /// Approximate schema/body cost in tokens; 0 for a cheap descriptor.
    #[serde(default)]
    pub load_cost: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<u64>,
}

impl ResourceDescriptor {
    pub fn bound(mut self) -> Self {
        self.title = truncate_chars(&self.title, RESOURCE_TITLE_MAX_CHARS);
        self.summary = truncate_chars(&self.summary, RESOURCE_SUMMARY_MAX_CHARS);
        self
    }

    pub fn model_line(&self) -> String {
        format!(
            "{} | {} | owner={} | {}",
            self.r#ref.uri(),
            self.lifecycle,
            self.owner,
            self.summary
        )
    }

    pub fn from_context(entry: &ExternalizedContext) -> Self {
        Self {
            r#ref: ResourceRef::context(entry.item_id, entry.last_access_gc_epoch),
            title: entry.context_ref.summary.clone(),
            summary: entry.context_ref.summary.clone(),
            owner: entry.source.clone().unwrap_or_else(|| "context".into()),
            lifecycle: format!("{:?}", entry.residency),
            permission_needs: Vec::new(),
            load_cost: 0,
            freshness: Some(entry.last_access_tick),
        }
        .bound()
    }

    pub fn from_tool(entry: &ToolCatalogEntry, catalog_revision: Option<u64>) -> Self {
        Self {
            r#ref: ResourceRef::tool(&entry.name, catalog_revision),
            title: entry.name.clone(),
            summary: entry.description.clone(),
            owner: entry.owner.clone(),
            lifecycle: entry.state.as_str().to_string(),
            permission_needs: Vec::new(),
            load_cost: 0,
            freshness: None,
        }
        .bound()
    }
}

/// Why a targeted inspect/resolve did not return a live descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "miss")]
pub enum DiscoveryMiss {
    NotFound,
    ProviderUnavailable {
        reason: String,
    },
    StaleRevision {
        requested: String,
        current: String,
    },
    Denied {
        reason: String,
    },
    /// The id exists but is not current evidence (terminal semantic, etc.).
    EvidenceAbsent {
        reason: String,
    },
}

impl DiscoveryMiss {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ProviderUnavailable { .. } => "provider_unavailable",
            Self::StaleRevision { .. } => "stale_revision",
            Self::Denied { .. } => "denied",
            Self::EvidenceAbsent { .. } => "evidence_absent",
        }
    }

    pub fn to_metadata(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({ "miss": self.code() }))
    }
}

/// Internal federated search request. Public tools map onto this; they do
/// not expose a `runtime.search` schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryQuery {
    pub query: String,
    pub kinds: Vec<ResourceKind>,
    pub limit: usize,
}

impl DiscoveryQuery {
    pub fn clamp(mut self) -> Self {
        self.query = self.query.chars().take(DISCOVERY_MAX_QUERY_CHARS).collect();
        let mut seen = HashSet::new();
        self.kinds.retain(|kind| seen.insert(*kind));
        self.kinds.truncate(DISCOVERY_MAX_FANOUT);
        if self.kinds.is_empty() {
            self.kinds = vec![ResourceKind::Context, ResourceKind::Tool];
        }
        if self.limit == 0 || self.limit > DISCOVERY_MAX_ROWS {
            self.limit = DISCOVERY_MAX_ROWS;
        }
        self
    }

    pub fn fingerprint(&self) -> u64 {
        let mut hasher = Fnv64::new();
        self.query.to_lowercase().hash(&mut hasher);
        let mut kinds = self.kinds.clone();
        kinds.sort_by_key(|kind| kind.as_str());
        kinds.hash(&mut hasher);
        hasher.finish()
    }
}

/// Why a per-turn discovery search was refused before dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryBudgetExhausted {
    QueryCount,
    IdenticalQuery,
}

impl DiscoveryBudgetExhausted {
    pub fn code(self) -> &'static str {
        match self {
            Self::QueryCount => "discovery.query_budget",
            Self::IdenticalQuery => "discovery.identical_query_budget",
        }
    }
}

/// Actor-owned per-user-turn search admission (`CTX-DISC-03`). Reset on
/// `start_turn`. Inspect/load/admit/fetch are not searches.
#[derive(Debug, Default, Clone)]
pub struct DiscoveryTurnBudget {
    queries: u32,
    identical: HashMap<u64, u32>,
}

impl DiscoveryTurnBudget {
    pub fn reset(&mut self) {
        self.queries = 0;
        self.identical.clear();
    }

    /// Consume one search slot, or refuse without executing.
    pub fn admit(&mut self, query: &DiscoveryQuery) -> Result<(), DiscoveryBudgetExhausted> {
        if self.queries >= DISCOVERY_MAX_QUERIES_PER_TURN {
            return Err(DiscoveryBudgetExhausted::QueryCount);
        }
        let fingerprint = query.fingerprint();
        let seen = self.identical.get(&fingerprint).copied().unwrap_or(0);
        if seen >= DISCOVERY_IDENTICAL_QUERY_BUDGET {
            return Err(DiscoveryBudgetExhausted::IdenticalQuery);
        }
        self.queries += 1;
        self.identical.insert(fingerprint, seen + 1);
        Ok(())
    }

    pub fn queries_this_turn(&self) -> u32 {
        self.queries
    }
}

/// Merged, bounded discovery page. `truncated` is true when a cap dropped
/// descriptors that providers returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryPage {
    pub descriptors: Vec<ResourceDescriptor>,
    pub truncated: bool,
}

impl DiscoveryPage {
    pub fn model_content(&self) -> String {
        if self.descriptors.is_empty() {
            return "no resources match".into();
        }
        let mut lines: Vec<String> = self
            .descriptors
            .iter()
            .map(ResourceDescriptor::model_line)
            .collect();
        let mut text = lines.join("\n");
        while text.chars().count() > DISCOVERY_MAX_RESULT_CHARS {
            if lines.len() <= 1 {
                text = truncate_chars(&text, DISCOVERY_MAX_RESULT_CHARS);
                break;
            }
            lines.pop();
            text = lines.join("\n");
        }
        text
    }
}

/// Shared internal planner: Context hits (already ranked) then Tool hits
/// (already ranked), clamped to the query limit and result-char cap.
pub fn federate(
    query: &DiscoveryQuery,
    context_hits: Vec<ResourceDescriptor>,
    tool_hits: Vec<ResourceDescriptor>,
) -> DiscoveryPage {
    let query = query.clone().clamp();
    let include_context = query.kinds.contains(&ResourceKind::Context);
    let include_tool = query.kinds.contains(&ResourceKind::Tool);
    let mut descriptors = Vec::new();
    if include_context {
        descriptors.extend(context_hits);
    }
    if include_tool {
        descriptors.extend(tool_hits);
    }
    let truncated = descriptors.len() > query.limit;
    descriptors.truncate(query.limit);
    let mut page = DiscoveryPage {
        descriptors,
        truncated,
    };
    let rendered = page.model_content();
    if rendered.chars().count() > DISCOVERY_MAX_RESULT_CHARS {
        page.truncated = true;
        while page.descriptors.len() > 1
            && page.model_content().chars().count() > DISCOVERY_MAX_RESULT_CHARS
        {
            page.descriptors.pop();
        }
    }
    page
}

/// Provider-owned tool catalog index (`TOOLS-10`). Candidate generation
/// uses token/owner/state/risk buckets; a needle that hits no key
/// residual-scans name+description so coverage stays at least as wide as
/// the old case-sensitive name-contains scan, but case-insensitive and
/// over descriptor fields.
pub fn search_tool_catalog(
    entries: &[ToolCatalogEntry],
    query: Option<&str>,
    limit: usize,
) -> Vec<ToolCatalogEntry> {
    let limit = if limit == 0 { entries.len() } else { limit };
    let Some(raw) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return entries.iter().take(limit).cloned().collect();
    };
    let needle = raw.to_lowercase();
    let index = ToolCatalogIndex::build(entries);
    let ids = index.candidate_ids(&needle);
    let mut hits = Vec::new();
    match ids {
        Some(ids) => {
            for idx in ids {
                if let Some(entry) = entries.get(idx)
                    && descriptor_matches(entry, &needle)
                {
                    hits.push(entry.clone());
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
        }
        None => {
            for entry in entries {
                if descriptor_matches(entry, &needle) {
                    hits.push(entry.clone());
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    hits
}

struct ToolCatalogIndex {
    by_token: HashMap<String, Vec<usize>>,
}

impl ToolCatalogIndex {
    fn build(entries: &[ToolCatalogEntry]) -> Self {
        let mut by_token: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, entry) in entries.iter().enumerate() {
            for token in tool_tokens(entry) {
                let bucket = by_token.entry(token).or_default();
                if bucket.last().copied() != Some(idx) {
                    bucket.push(idx);
                }
            }
        }
        Self { by_token }
    }

    fn candidate_ids(&self, needle: &str) -> Option<Vec<usize>> {
        let mut ids: Option<HashSet<usize>> = None;
        let mut any_token = false;
        for token in tokenize(needle) {
            any_token = true;
            let mut bucket: HashSet<usize> = HashSet::new();
            for (key, rows) in &self.by_token {
                if key.contains(&token) {
                    bucket.extend(rows.iter().copied());
                }
            }
            ids = Some(match ids.take() {
                None => bucket,
                Some(set) => set.intersection(&bucket).copied().collect(),
            });
        }
        if !any_token {
            return None;
        }
        let Some(set) = ids else {
            return None;
        };
        if set.is_empty() {
            return None;
        }
        let mut ids: Vec<usize> = set.into_iter().collect();
        ids.sort_unstable();
        Some(ids)
    }
}

fn descriptor_matches(entry: &ToolCatalogEntry, needle: &str) -> bool {
    entry.name.to_lowercase().contains(needle)
        || entry.description.to_lowercase().contains(needle)
        || entry.owner.to_lowercase().contains(needle)
        || entry.state.as_str().contains(needle)
        || entry.risk.as_str().contains(needle)
}

fn tool_tokens(entry: &ToolCatalogEntry) -> HashSet<String> {
    let mut tokens = tokenize(&entry.name);
    tokens.extend(tokenize(&entry.description));
    tokens.extend(tokenize(&entry.owner));
    tokens.insert(entry.state.as_str().to_string());
    tokens.insert(entry.risk.as_str().to_string());
    tokens
}

fn tokenize(text: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.insert(current);
    }
    tokens
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }
}

impl Hasher for Fnv64 {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.wrapping_mul(0x100_0000_01b3) ^ u64::from(byte);
        }
    }
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Parse a manage-tool call into an internal discovery search, if it is
/// one. Inspect/load/admit/fetch are not searches.
pub fn discovery_search_from_call(
    tool_name: &str,
    arguments: &serde_json::Value,
) -> Option<DiscoveryQuery> {
    let op = arguments.get("op")?.as_str()?;
    if op != "search" {
        return None;
    }
    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let limit = arguments.get("limit").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let kinds = match tool_name {
        crate::CONTEXT_MANAGE => vec![ResourceKind::Context],
        crate::CAPABILITY_MANAGE => vec![ResourceKind::Tool],
        _ => return None,
    };
    Some(
        DiscoveryQuery {
            query,
            kinds,
            limit,
        }
        .clamp(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContextKind, ContextRef, ContextResidency, ContextRetention, ContextScope, SemanticState,
        ToolLifecycle, ToolRisk,
    };

    fn tool(name: &str, description: &str, owner: &str) -> ToolCatalogEntry {
        ToolCatalogEntry {
            name: name.into(),
            state: ToolLifecycle::Available,
            owner: owner.into(),
            description: description.into(),
            risk: ToolRisk::ReadOnly,
        }
    }

    #[test]
    fn resource_ref_roundtrips_uri() {
        let id = ContextItemId::new();
        let parsed = ResourceRef::parse(&ResourceRef::context(id, Some(3)).uri()).unwrap();
        assert_eq!(parsed.kind, ResourceKind::Context);
        assert_eq!(parsed.id, id.to_string());
        assert_eq!(parsed.revision.as_deref(), Some("3"));
        let tool = ResourceRef::parse(&ResourceRef::tool("fs.read", None).uri()).unwrap();
        assert_eq!(tool.kind, ResourceKind::Tool);
        assert_eq!(tool.id, "fs.read");
    }

    #[test]
    fn tool_search_matches_description_case_insensitively() {
        let entries = vec![
            tool("fs.read", "Read a workspace file", "builtin"),
            tool("git.status", "Show the working tree status", "builtin"),
        ];
        let hits = search_tool_catalog(&entries, Some("WORKING TREE"), 8);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "git.status");
        let by_owner = search_tool_catalog(&entries, Some("BuiltIn"), 8);
        assert_eq!(by_owner.len(), 2);
    }

    #[test]
    fn tool_search_empty_query_returns_the_catalog_prefix() {
        let entries = vec![tool("a", "A", "builtin"), tool("b", "B", "builtin")];
        let hits = search_tool_catalog(&entries, None, 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "a");
    }

    #[test]
    fn federate_is_context_then_tool_and_respects_limit() {
        let context = vec![ResourceDescriptor::from_context(&ExternalizedContext {
            item_id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: crate::AttentionState::Archived,
            semantic: SemanticState::Live,
            context_ref: ContextRef {
                uri: "context://run/x".into(),
                item_id: ContextItemId::new(),
                kind: ContextKind::Note,
                scope: ContextScope::Task,
                summary: "alpha".into(),
                created_tick: 0,
            },
            externalized_at_tick: 0,
            last_access_tick: 0,
            residency: ContextResidency::Cold,
            entities: Vec::new(),
            tags: Vec::new(),
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            last_access_gc_epoch: Some(1),
            blob_checksum: None,
            source: None,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            access_count: 0,
            last_access_signal: crate::AccessSignal::None,
            search_reinforce_count: 0,
            gc_generation: 0,
            evicted_at_tick: None,
        })];
        let tools = vec![ResourceDescriptor::from_tool(
            &tool("fs.read", "read files", "builtin"),
            None,
        )];
        let page = federate(
            &DiscoveryQuery {
                query: "x".into(),
                kinds: vec![ResourceKind::Context, ResourceKind::Tool],
                limit: 1,
            },
            context,
            tools,
        );
        assert_eq!(page.descriptors.len(), 1);
        assert!(page.truncated);
        assert_eq!(page.descriptors[0].r#ref.kind, ResourceKind::Context);
    }

    #[test]
    fn discovery_search_from_call_ignores_inspect_and_load() {
        assert!(
            discovery_search_from_call(
                crate::CAPABILITY_MANAGE,
                &serde_json::json!({"op": "inspect", "name": "fs.read"})
            )
            .is_none()
        );
        let search = discovery_search_from_call(
            crate::CONTEXT_MANAGE,
            &serde_json::json!({"op": "search", "query": "Auth", "limit": 4}),
        )
        .unwrap();
        assert_eq!(search.kinds, vec![ResourceKind::Context]);
        assert_eq!(search.query, "Auth");
    }

    #[test]
    fn discovery_turn_budget_caps_count_and_identical_queries() {
        let mut budget = DiscoveryTurnBudget::default();
        let query = DiscoveryQuery {
            query: "auth".into(),
            kinds: vec![ResourceKind::Context],
            limit: 8,
        }
        .clamp();
        assert!(budget.admit(&query).is_ok());
        assert!(budget.admit(&query).is_ok());
        assert_eq!(
            budget.admit(&query),
            Err(DiscoveryBudgetExhausted::IdenticalQuery)
        );
        budget.reset();
        for _ in 0..DISCOVERY_MAX_QUERIES_PER_TURN {
            let distinct = DiscoveryQuery {
                query: format!("q{}", budget.queries_this_turn()),
                kinds: vec![ResourceKind::Tool],
                limit: 4,
            }
            .clamp();
            assert!(budget.admit(&distinct).is_ok());
        }
        let overflow = DiscoveryQuery {
            query: "overflow".into(),
            kinds: vec![ResourceKind::Tool],
            limit: 4,
        }
        .clamp();
        assert_eq!(
            budget.admit(&overflow),
            Err(DiscoveryBudgetExhausted::QueryCount)
        );
    }
}
