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

/// Provider-owned tool catalog search (`TOOLS-10`).
///
/// Ranking is token-OR plus a phrase bonus: a multi-word needle such as
/// `"patch edit file"` must still hit `edit.patch`. Requiring every token
/// *and* the whole phrase as a substring dropped those queries to zero
/// hits. Empty query returns the catalog prefix (stable name order).
pub fn search_tool_catalog(
    entries: &[ToolCatalogEntry],
    query: Option<&str>,
    limit: usize,
) -> Vec<ToolCatalogEntry> {
    search_tool_catalog_filtered(entries, query, None, limit)
}

/// Catalog search with an optional [`crate::ToolSemanticRole`] filter.
///
/// `role=Mutate` is the execution-gap path: the model asks for a mutation
/// primitive instead of guessing `"patch edit file"`. Role filtering
/// happens before text ranking. Empty query + role returns the matching
/// catalog prefix (stable name order).
pub fn search_tool_catalog_filtered(
    entries: &[ToolCatalogEntry],
    query: Option<&str>,
    role: Option<crate::ToolSemanticRole>,
    limit: usize,
) -> Vec<ToolCatalogEntry> {
    let limit = if limit == 0 { entries.len() } else { limit };
    let filtered: Vec<&ToolCatalogEntry> = match role {
        Some(role) => entries
            .iter()
            .filter(|entry| entry.has_role(role))
            .collect(),
        None => entries.iter().collect(),
    };
    let Some(raw) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return filtered.into_iter().take(limit).cloned().collect();
    };
    let needle = raw.to_lowercase();
    let tokens: Vec<String> = tokenize(&needle).into_iter().collect();
    let mut scored: Vec<(u32, usize)> = Vec::new();
    for (idx, entry) in filtered.iter().enumerate() {
        let score = catalog_search_score(entry, &needle, &tokens);
        if score > 0 {
            scored.push((score, idx));
        }
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| filtered[a.1].name.cmp(&filtered[b.1].name))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, idx)| filtered[idx].clone())
        .collect()
}

fn catalog_search_score(entry: &ToolCatalogEntry, needle: &str, tokens: &[String]) -> u32 {
    let mut score = 0u32;
    if descriptor_matches(entry, needle) {
        score += 1000;
    }
    let name = entry.name.to_lowercase();
    if name == needle || name.contains(needle) {
        score += 200;
    }
    let haystack = tool_tokens(entry);
    for token in tokens {
        if token_hits_entry(token, &haystack, entry) {
            score += 10;
        }
    }
    score
}

fn token_hits_entry(token: &str, haystack: &HashSet<String>, entry: &ToolCatalogEntry) -> bool {
    // Haystack-contains-query covers stems (`files` vs `file`). Never the
    // reverse: a 1-char haystack token would match every query.
    if haystack
        .iter()
        .any(|h| h == token || (token.len() >= 2 && h.contains(token)))
    {
        return true;
    }
    descriptor_matches(entry, token)
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

/// Bounded model-facing catalog index: tools *not* on this round's schema
/// surface. Names only — full JSON schemas stay behind `capability.manage`
/// load. Surfaced tools are omitted so the index does not duplicate the
/// tools array. Empty when every catalog entry is already on the surface.
pub fn render_tool_catalog_index(
    entries: &[ToolCatalogEntry],
    surfaced: &HashSet<&str>,
) -> Option<String> {
    let mut rows: Vec<&ToolCatalogEntry> = entries
        .iter()
        .filter(|entry| {
            entry.name != crate::CAPABILITY_MANAGE && !surfaced.contains(entry.name.as_str())
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.truncate(crate::MAX_TOOL_CATALOG_INDEX_ROWS);
    if rows.is_empty() {
        return None;
    }
    let mut out = String::from("tool_catalog/v1\nload via capability.manage op=load name=<id>\n");
    for entry in rows {
        let summary = truncate_chars(
            entry.description.trim(),
            crate::MAX_TOOL_CATALOG_INDEX_SUMMARY_CHARS,
        );
        let line = format!("{}\t{}\t{summary}", entry.state.as_str(), entry.name);
        if out.len() + line.len() + 1 > crate::MAX_TOOL_CATALOG_INDEX_CHARS {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    Some(out)
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

/// One-line model-facing purpose for a catalog row: the first sentence of
/// the description, truncated. Shared by `capability.manage` search so a
/// discovery hit answers "what does this tool do" inline — the model
/// chains straight to load/invoke instead of spending an `inspect` round.
/// `capability.manage op=inspect` remains the full-card path.
pub fn compact_tool_purpose(description: &str) -> String {
    let trimmed = description.trim();
    let purpose = match trimmed.find(". ") {
        Some(idx) => &trimmed[..idx + 1],
        None => trimmed,
    };
    truncate_chars(purpose, crate::MAX_TOOL_SURFACE_DESCRIPTION_CHARS)
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
    use std::collections::HashSet;

    fn tool(name: &str, description: &str, owner: &str) -> ToolCatalogEntry {
        ToolCatalogEntry {
            name: name.into(),
            state: ToolLifecycle::Available,
            owner: owner.into(),
            description: description.into(),
            risk: ToolRisk::ReadOnly,
            roles: Vec::new(),
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
    fn tool_search_ranks_multi_word_needles_without_requiring_the_whole_phrase() {
        let entries = vec![
            tool("fs.read", "Read a workspace file", "builtin"),
            tool(
                "edit.patch",
                "Apply exact-match text hunks to one or more workspace files",
                "builtin",
            ),
            tool("fs.write", "Write/replace a UTF-8 text file", "builtin"),
        ];
        let hits = search_tool_catalog(&entries, Some("patch edit file"), 8);
        assert_eq!(hits[0].name, "edit.patch");
        assert!(hits.iter().any(|hit| hit.name == "fs.read"));
    }

    #[test]
    fn tool_search_by_mutate_role_returns_edit_patch_without_a_text_query() {
        use crate::ToolSemanticRole;
        let entries = vec![
            tool("fs.read", "Read a workspace file", "builtin"),
            tool(
                "edit.patch",
                "Apply exact-match text hunks to one or more workspace files",
                "builtin",
            ),
            tool("git.status", "Show the working tree status", "builtin"),
            tool("shell.exec", "Run a shell command", "builtin"),
        ];
        let hits = search_tool_catalog_filtered(&entries, None, Some(ToolSemanticRole::Mutate), 8);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "edit.patch");
        let hits = search_tool_catalog_filtered(
            &entries,
            Some("patch"),
            Some(ToolSemanticRole::Mutate),
            8,
        );
        assert_eq!(hits[0].name, "edit.patch");
        let empty = search_tool_catalog_filtered(
            &entries,
            Some("status"),
            Some(ToolSemanticRole::Mutate),
            8,
        );
        assert!(
            empty.is_empty(),
            "role filter must not leak InspectDiff tools: {:?}",
            empty.iter().map(|h| h.name.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tool_catalog_index_omits_surfaced_names_and_stays_bounded() {
        let entries = vec![
            tool("fs.read", "Read a workspace file", "builtin"),
            tool("edit.patch", "Apply exact-match text hunks", "builtin"),
        ];
        let surfaced = HashSet::from(["fs.read"]);
        let rendered = render_tool_catalog_index(&entries, &surfaced).expect("index");
        assert!(rendered.starts_with("tool_catalog/v1"));
        assert!(rendered.contains("edit.patch"));
        assert!(!rendered.contains("fs.read"));
        assert!(rendered.len() <= crate::MAX_TOOL_CATALOG_INDEX_CHARS);
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
            file_path: None,
            file_revision: None,
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
