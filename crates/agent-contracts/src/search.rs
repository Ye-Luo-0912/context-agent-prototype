//! Shared deterministic text-search kernel — the common foundation for the
//! project's independent search services (tool catalog search, context
//! catalog search, and future Skill/artifact providers).
//!
//! Division of labor: each searcher owns its domain (which fields are
//! indexed, which filters exist, how results tie-break); this module owns
//! only the mechanics they would otherwise each reimplement —
//! tokenization, a bounded inverted index, and a deterministic candidate
//! score. Every feature is explicit and explainable: token overlap,
//! document-frequency rarity tiers, and unique-prefix extension. There is
//! deliberately no embedding, fuzzy scoring, or learned weight here (v0
//! freeze): same input, same order, forever.

use std::collections::{HashMap, HashSet};

use crate::ids::ContextItemId;

/// Why a candidate set may be incomplete against the full corpus
/// (SCHED-02). Each reason names the bound that hid potential matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIncompleteReason {
    /// A queried token's posting list was capped at
    /// [`MAX_POSTINGS_PER_TOKEN`]; later documents were rejected.
    SaturatedPosting,
    /// Some indexed docs truncated their text at the index prefix bound,
    /// so keywords beyond it cannot match inside the index.
    TruncatedIndexedText,
}

/// Candidate ids plus an explicit completeness statement. Search is the
/// GC safety net: when `incomplete` is set, callers must run a bounded
/// residual verification over non-candidates instead of trusting the set.
#[derive(Debug, Clone, Default)]
pub struct SearchCandidates {
    pub ids: Vec<ContextItemId>,
    pub incomplete: Option<SearchIncompleteReason>,
}

/// Tokens shorter than this are stop-characters noise (`rs`, `a`), not
/// needles. Kept small so version-ish fragments still match.
pub const MIN_TOKEN_CHARS: usize = 2;
/// One document may contribute at most this many distinct tokens. Long
/// bodies simply stop feeding the index past the cap; they stay reachable
/// through their leading tokens.
pub const MAX_TOKENS_PER_DOC: usize = 64;
/// Postings per token are hard-capped so one ubiquitous token cannot grow
/// the index without bound. Beyond the cap the token keeps matching its
/// earliest documents only; `stats()` surfaces saturation.
pub const MAX_POSTINGS_PER_TOKEN: usize = 4096;

/// Document-frequency rarity tiers. Rare tokens discriminate; common ones
/// merely co-occur. Static thresholds, not learned weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TokenRarity {
    Common,
    Uncommon,
    Rare,
}

impl TokenRarity {
    /// Points contributed to a document's rarity sum. Rare beats two
    /// uncommon hits; a common match adds nothing but coverage.
    pub fn points(self) -> u32 {
        match self {
            TokenRarity::Common => 0,
            TokenRarity::Uncommon => 1,
            TokenRarity::Rare => 2,
        }
    }

    fn for_df(df: usize) -> Self {
        match df {
            0..=1 => TokenRarity::Rare,
            2..=8 => TokenRarity::Uncommon,
            _ => TokenRarity::Common,
        }
    }
}

/// One candidate document under the kernel's deterministic score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoredMatch {
    /// Caller-assigned document handle.
    pub doc: u32,
    /// How many distinct query tokens this document matched. Higher wins.
    pub coverage: usize,
    /// Sum of [`TokenRarity::points`] over the matched tokens. Higher wins.
    pub rarity: u32,
    /// True when at least one match came from a unique-prefix extension
    /// rather than an exact dictionary hit (weaker evidence).
    pub prefix_extended: bool,
}

/// Deterministic tokenizer: lowercase, split on non-alphanumeric
/// characters, drop short fragments, dedupe, cap per call. Order is
/// first-appearance so downstream iteration is reproducible.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            push_token(&mut tokens, &current);
            current.clear();
        }
    }
    if !current.is_empty() {
        push_token(&mut tokens, &current);
    }
    tokens
}

fn push_token(tokens: &mut Vec<String>, token: &str) {
    if token.chars().count() < MIN_TOKEN_CHARS || tokens.len() >= MAX_TOKENS_PER_DOC {
        return;
    }
    if !tokens.iter().any(|existing| existing == token) {
        tokens.push(token.to_string());
    }
}

/// Bounded inverted index over caller-assigned document handles.
///
/// Mechanics only: callers decide what text a document carries and what
/// its id means. Insertion is incremental; removal walks the reverse map
/// in O(tokens per doc); a saturated token simply stops accepting
/// postings instead of growing without bound.
#[derive(Debug, Default)]
pub struct TextIndex {
    postings: HashMap<String, Vec<u32>>,
    reverse: HashMap<u32, Vec<String>>,
    /// Distinct dictionary tokens that stopped accepting postings at the
    /// cap. A set, not a counter: every rejected insertion against the
    /// same token is one saturated token, not N.
    saturated: HashSet<String>,
}

impl TextIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index one document under all tokens of the given fields. Re-inserting
    /// an existing doc is a no-op (remove first to reindex). Returns whether
    /// the document was newly indexed.
    pub fn insert(&mut self, doc: u32, fields: &[&str]) -> bool {
        if self.reverse.contains_key(&doc) {
            return false;
        }
        // Early-exit at the cap: the dropped tail is exactly what the old
        // post-loop truncate discarded, so semantics are identical while
        // cross-field dedup stays bounded instead of O(all field tokens²).
        let mut tokens: Vec<String> = Vec::new();
        'fields: for field in fields {
            for token in tokenize(field) {
                if !tokens.contains(&token) {
                    tokens.push(token);
                    if tokens.len() >= MAX_TOKENS_PER_DOC {
                        break 'fields;
                    }
                }
            }
        }
        for token in &tokens {
            let posting = self.postings.entry(token.clone()).or_default();
            if posting.len() >= MAX_POSTINGS_PER_TOKEN {
                self.saturated.insert(token.clone());
                continue;
            }
            posting.push(doc);
        }
        self.reverse.insert(doc, tokens);
        true
    }

    /// Remove a document from every posting list in O(tokens per doc).
    pub fn remove(&mut self, doc: u32) {
        if let Some(tokens) = self.reverse.remove(&doc) {
            for token in tokens {
                if let Some(posting) = self.postings.get_mut(&token) {
                    posting.retain(|existing| *existing != doc);
                    if posting.is_empty() {
                        self.postings.remove(&token);
                    }
                }
            }
        }
    }

    pub fn contains(&self, doc: u32) -> bool {
        self.reverse.contains_key(&doc)
    }

    /// Drop every document (the wholesale-rebuild path).
    pub fn clear(&mut self) {
        self.postings.clear();
        self.reverse.clear();
        self.saturated.clear();
    }

    pub fn len(&self) -> usize {
        self.reverse.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reverse.is_empty()
    }

    /// Distinct tokens in the dictionary.
    pub fn dictionary_len(&self) -> usize {
        self.postings.len()
    }

    /// Tokens that stopped accepting postings at the cap.
    pub fn saturated_tokens(&self) -> usize {
        self.saturated.len()
    }

    /// Whether any of `tokens` hit the posting cap at insert time. A yes
    /// means this search's candidate set may be missing documents whose
    /// postings were rejected — callers must treat recall as partial.
    pub fn has_saturated_token(&self, tokens: &[String]) -> bool {
        tokens.iter().any(|token| self.saturated.contains(token))
    }

    /// Score every document matching the query tokens and return the
    /// candidates in deterministic order: coverage descending, rarity sum
    /// descending, doc handle ascending. An empty query matches nothing —
    /// browsing is the caller's job.
    ///
    /// A query token with no exact dictionary hit tries one unique-prefix
    /// extension: if exactly one dictionary token extends it, those
    /// postings join at [`TokenRarity::Common`] strength with
    /// `prefix_extended` set. Ambiguous prefixes match nothing, so the
    /// extension sharpens rather than blurs. The extension scan is one
    /// pass over the dictionary per unmatched query token — bounded by
    /// vocabulary size, not corpus size; callers indexing large corpora
    /// should keep queries token-shaped so the exact-hit path dominates.
    pub fn search(&self, query: &str) -> Vec<ScoredMatch> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        // Per matched document: coverage count, rarity sum, prefix flag.
        let mut coverage: HashMap<u32, usize> = HashMap::new();
        let mut rarity: HashMap<u32, u32> = HashMap::new();
        let mut prefix_extended: HashMap<u32, bool> = HashMap::new();

        let record = |doc: u32,
                      points: u32,
                      prefix: bool,
                      coverage: &mut HashMap<u32, usize>,
                      rarity: &mut HashMap<u32, u32>,
                      prefix_extended: &mut HashMap<u32, bool>| {
            *coverage.entry(doc).or_insert(0) += 1;
            *rarity.entry(doc).or_insert(0) += points;
            if prefix {
                prefix_extended.insert(doc, true);
            }
        };

        for token in &query_tokens {
            match self.postings.get(token) {
                Some(posting) => {
                    let points = TokenRarity::for_df(posting.len()).points();
                    for &doc in posting {
                        record(
                            doc,
                            points,
                            false,
                            &mut coverage,
                            &mut rarity,
                            &mut prefix_extended,
                        );
                    }
                }
                None => {
                    if let Some((extended, posting)) = self.unique_prefix_extension(token) {
                        for &doc in posting {
                            record(
                                doc,
                                TokenRarity::Common.points(),
                                true,
                                &mut coverage,
                                &mut rarity,
                                &mut prefix_extended,
                            );
                        }
                        let _ = extended;
                    }
                }
            }
        }

        let mut matches: Vec<ScoredMatch> = coverage
            .into_iter()
            .map(|(doc, covered)| ScoredMatch {
                doc,
                coverage: covered,
                rarity: rarity.get(&doc).copied().unwrap_or(0),
                prefix_extended: prefix_extended.get(&doc).copied().unwrap_or(false),
            })
            .collect();
        matches.sort_by(|left, right| {
            right
                .coverage
                .cmp(&left.coverage)
                .then_with(|| right.rarity.cmp(&left.rarity))
                .then_with(|| left.doc.cmp(&right.doc))
        });
        matches
    }

    /// The one dictionary token uniquely extended by `needle`, if any.
    fn unique_prefix_extension(&self, needle: &str) -> Option<(&str, &Vec<u32>)> {
        if needle.chars().count() < MIN_TOKEN_CHARS {
            return None;
        }
        let mut found: Option<(&str, &Vec<u32>)> = None;
        for (key, posting) in &self.postings {
            if key.starts_with(needle) {
                if found.is_some() {
                    return None;
                }
                found = Some((key.as_str(), posting));
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_is_lowercase_split_and_deduped() {
        assert_eq!(
            tokenize("Fix AuthService.rs:42"),
            vec!["fix", "authservice", "rs", "42"]
        );
        assert_eq!(
            tokenize("use TOML for config"),
            vec!["use", "toml", "for", "config"]
        );
        assert_eq!(
            tokenize("AUTH auth Auth"),
            vec!["auth"],
            "case-folded and deduped"
        );
        assert!(
            tokenize("a :: b").is_empty(),
            "fragments below the minimum length are dropped"
        );
    }

    #[test]
    fn tokenize_caps_per_document() {
        let body: String = (0..200).map(|i| format!("tok{i} ")).collect();
        assert_eq!(tokenize(&body).len(), MAX_TOKENS_PER_DOC);
    }

    #[test]
    fn coverage_ranks_multi_token_hits_above_single() {
        let mut index = TextIndex::new();
        index.insert(1, &["auth service timeout fix"]);
        index.insert(2, &["timeout in the cache layer"]);

        let hits = index.search("auth timeout");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc, 1, "both tokens beat one token");
        assert_eq!(hits[0].coverage, 2);
        assert_eq!(hits[1].doc, 2);
        assert_eq!(hits[1].coverage, 1);
    }

    #[test]
    fn rare_tokens_outrank_common_ones_at_equal_coverage() {
        let mut index = TextIndex::new();
        // `config` appears in five docs (uncommon); `zeta` only in doc 9 (rare).
        for doc in 1..=4u32 {
            index.insert(doc, &["config noise"]);
        }
        index.insert(5, &["config zeta"]);

        let hits = index.search("config zeta");
        assert_eq!(hits[0].doc, 5, "equal coverage: the rare token wins");
        assert!(hits[0].rarity > hits[1].rarity);
    }

    #[test]
    fn unique_prefix_extends_but_ambiguous_prefix_does_not() {
        let mut index = TextIndex::new();
        index.insert(1, &["authservice"]);
        index.insert(2, &["cachestore", "cacheservice"]);

        let hits = index.search("authserv");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc, 1);
        assert!(hits[0].prefix_extended);

        // Two dictionary tokens extend `cache`, so the prefix is ambiguous
        // and contributes nothing; the exact `cacheservice` token still
        // matches without the prefix flag.
        let hits = index.search("cacheservice store");
        assert!(hits.iter().all(|hit| !hit.prefix_extended));
        assert_eq!(hits[0].doc, 2);
        assert_eq!(hits[0].coverage, 1);
    }

    #[test]
    fn remove_drops_every_posting_and_reinsert_reindexes() {
        let mut index = TextIndex::new();
        index.insert(1, &["alpha beta"]);
        index.insert(2, &["alpha gamma"]);
        assert!(index.search("alpha").iter().any(|hit| hit.doc == 1));

        index.remove(1);
        assert!(!index.contains(1));
        let hits = index.search("beta");
        assert!(hits.is_empty(), "reverse map removal clears stale postings");
        assert_eq!(index.dictionary_len(), 2, "gamma and alpha remain");

        assert!(index.insert(1, &["alpha delta"]));
        assert_eq!(index.search("delta")[0].doc, 1);
    }

    #[test]
    fn duplicate_insert_is_a_no_op() {
        let mut index = TextIndex::new();
        assert!(index.insert(1, &["alpha"]));
        assert!(!index.insert(1, &["beta"]));
        assert!(
            index.search("beta").is_empty(),
            "re-insert must not silently reindex under new fields"
        );
    }

    #[test]
    fn empty_query_matches_nothing_and_order_is_total() {
        let mut index = TextIndex::new();
        index.insert(3, &["alpha"]);
        index.insert(1, &["alpha"]);
        index.insert(2, &["alpha"]);

        assert!(index.search("").is_empty());
        assert!(index.search("   ").is_empty());

        // Equal coverage, equal rarity: doc-handle ascending breaks the tie,
        // so the order is fully deterministic.
        let docs: Vec<u32> = index.search("alpha").into_iter().map(|h| h.doc).collect();
        assert_eq!(docs, vec![1, 2, 3]);
    }

    #[test]
    fn postings_cap_saturates_without_panicking() {
        let mut index = TextIndex::new();
        for doc in 0..(MAX_POSTINGS_PER_TOKEN + 16) {
            index.insert(doc as u32, &["ubiquitous"]);
        }
        assert_eq!(
            index.saturated_tokens(),
            1,
            "the metric counts distinct saturated tokens, not rejected postings"
        );
        let hits = index.search("ubiquitous");
        assert_eq!(hits.len(), MAX_POSTINGS_PER_TOKEN);
    }
}
