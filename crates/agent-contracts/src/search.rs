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

/// 候选集可能不完整的原因：对应是哪个索引上限藏住了潜在命中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchIncompleteReason {
    /// 查询词的倒排列表写入时触顶，后来的文档被拒绝。
    SaturatedPosting,
    /// 部分已索引文档的正文在索引前缀处截断，深处的关键词索引看不见。
    TruncatedIndexedText,
    /// 查询含被索引丢弃的短 token，或 tokenizer 无法表达稳定词界
    /// （例如中文）；候选索引无法证明无命中。
    UnindexedQueryShape,
}

/// 候选 id 加显式完备性说明。检索是 GC 的兜底网：`incomplete` 非空时，
/// 调用方必须对非候选做有界残差校验，不能默认集合完整。
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
/// Query tokenization is separately bounded. A normalized 256-character
/// query can contain at most 85 distinct two-character tokens separated by
/// delimiters, so this cap does not discard any token accepted at the public
/// search boundary.
pub const MAX_QUERY_TOKENS: usize = 128;
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
    tokenize_with_limit(text, MAX_TOKENS_PER_DOC, MIN_TOKEN_CHARS)
}

/// Tokenize a search query without applying the smaller per-document index
/// budget. This keeps final AND verification honest for normalized queries
/// containing more than 64 distinct tokens while remaining independently
/// bounded for direct callers.
pub fn tokenize_query(text: &str) -> Vec<String> {
    tokenize_with_limit(text, MAX_QUERY_TOKENS, MIN_TOKEN_CHARS)
}

/// Tokenize every alphanumeric query run, including one-character fragments.
/// Candidate indexes intentionally omit those fragments, but residual/final
/// verification must retain them so a mixed query cannot silently weaken.
pub fn tokenize_query_fragments(text: &str) -> Vec<String> {
    tokenize_with_limit(text, MAX_QUERY_TOKENS, 1)
}

/// Whether the word-token index cannot soundly bound this non-empty query.
/// Single-character fragments are dropped by design, while non-ASCII
/// alphanumeric runs do not provide reliable word boundaries for languages
/// such as Chinese. Callers must use a bounded residual path for both.
pub fn query_needs_text_residual(text: &str) -> bool {
    let text = text.trim();
    let indexed = tokenize_query(text);
    let fragments = tokenize_query_fragments(text);
    !text.is_empty()
        && (indexed.is_empty()
            || indexed.len() != fragments.len()
            || query_fragment_uses_substring(text))
}

/// Whether a query fragment uses a script for which this simple tokenizer has
/// no sound word-boundary model. Other Unicode alphabets (for example
/// Cyrillic and accented Latin) retain exact-token verification.
pub fn query_fragment_uses_substring(fragment: &str) -> bool {
    fragment.chars().any(|ch| {
        matches!(
            ch as u32,
            0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xF900..=0xFAFF
                | 0x20000..=0x2FA1F
                | 0x3040..=0x30FF
                | 0x31F0..=0x31FF
                | 0xAC00..=0xD7AF
        )
    })
}

fn tokenize_with_limit(text: &str, limit: usize, min_chars: usize) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            push_token(&mut tokens, &current, limit, min_chars);
            current.clear();
        }
    }
    if !current.is_empty() {
        push_token(&mut tokens, &current, limit, min_chars);
    }
    tokens
}

fn push_token(tokens: &mut Vec<String>, token: &str, limit: usize, min_chars: usize) {
    if token.chars().count() < min_chars || tokens.len() >= limit {
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

    /// Whether candidate generation for this query follows either an exact
    /// or unique-prefix posting list that saturated during insertion. This
    /// must resolve prefixes exactly like [`Self::search`]; checking only the
    /// literal query token would falsely call a capped extended posting
    /// complete.
    pub fn query_has_saturated_match(&self, query: &str) -> bool {
        tokenize_query(query).iter().any(|token| {
            if self.saturated.contains(token) {
                return true;
            }
            if self.postings.contains_key(token) {
                return false;
            }
            self.unique_prefix_key(token)
                .is_some_and(|extended| self.saturated.contains(extended))
        })
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
        let query_tokens = tokenize_query(query);
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
        let key = self.unique_prefix_key(needle)?;
        self.postings
            .get_key_value(key)
            .map(|(key, rows)| (key.as_str(), rows))
    }

    /// Resolve a unique prefix over both current postings and sticky saturated
    /// vocabulary. A saturated token remains evidence of rejected documents
    /// even if removal later empties its accepted posting list.
    fn unique_prefix_key(&self, needle: &str) -> Option<&str> {
        if needle.chars().count() < MIN_TOKEN_CHARS {
            return None;
        }
        let mut found: Option<&str> = None;
        for key in self.postings.keys().chain(self.saturated.iter()) {
            if key.starts_with(needle) {
                if found.is_some_and(|existing| existing != key) {
                    return None;
                }
                found = Some(key.as_str());
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
    fn query_tokenization_keeps_every_token_within_the_public_char_bound() {
        let query = (0..80)
            .map(|i| format!("{i:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(query.chars().count() <= 256);
        assert_eq!(tokenize(&query).len(), MAX_TOKENS_PER_DOC);
        assert_eq!(tokenize_query(&query).len(), 80);

        let mut index = TextIndex::new();
        index.insert(7, &["4f"]);
        assert_eq!(
            index
                .search(&query)
                .into_iter()
                .map(|hit| hit.doc)
                .collect::<Vec<_>>(),
            vec![7],
            "candidate generation must not discard the query's token after position 64"
        );
    }

    #[test]
    fn query_shape_marks_short_fragments_and_cjk_for_residual_verification() {
        assert!(query_needs_text_residual("界"));
        assert!(query_needs_text_residual("世界"));
        assert!(query_needs_text_residual("a zebra"));
        assert!(query_needs_text_residual("界 zebra"));
        assert!(query_needs_text_residual("_"));
        assert_eq!(tokenize_query_fragments("a zebra"), ["a", "zebra"]);
        assert!(!query_needs_text_residual("кот"));
        assert!(!query_needs_text_residual("café"));
        assert!(!query_needs_text_residual("authservice"));
        assert!(!query_needs_text_residual("auth service"));
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
        assert!(index.query_has_saturated_match("ubiquitous"));
        assert!(
            index.query_has_saturated_match("ubiq"),
            "the same unique-prefix resolution must carry saturation"
        );

        for doc in 0..MAX_POSTINGS_PER_TOKEN {
            index.remove(doc as u32);
        }
        assert!(index.search("ubiquitous").is_empty());
        assert!(
            index.query_has_saturated_match("ubiquitous"),
            "sticky saturation survives removal of every accepted posting"
        );
        assert!(
            index.query_has_saturated_match("ubiq"),
            "the saturated vocabulary also preserves prefix incompleteness"
        );
    }
}
