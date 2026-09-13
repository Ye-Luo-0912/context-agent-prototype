use agent_contracts::{
    AttentionState, ContextItem, ContextItemId, ContextKind, ContextStateTransition, CoreLabel,
    LifecycleLabel, SemanticState,
};

use crate::engine::State;
use crate::index::entity::{
    entities_match_exact, extract_entities, is_file_body_entry, is_file_body_observation,
    is_file_path_entity, observation_file_path,
};

/// A user message reads as a decision when it carries a directive verb
/// ("use X", "switch to Y", "revert", "drop Z", ...). Explicit, keyword
/// based, explainable — no learned scoring.
pub(crate) fn classify_decision(text: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "use ",
        "switch",
        "revert",
        "drop ",
        "adopt",
        "prefer",
        "instead of",
        "no, ",
        "actually ",
        "replace ",
        "remove ",
    ];
    let lower = text.to_lowercase();
    KEYWORDS.iter().any(|keyword| lower.contains(keyword))
}

/// Cues that make the replacement of an earlier decision *explicit*. A
/// plain "use X" / "prefer X" / "adopt X" adds a constraint next to the
/// existing ones; these cues say the earlier line is being withdrawn
/// ("switch to Y instead of X", "use Y instead", "drop the TOML decision",
/// "actually, no, ..."). Keyword based and explainable, like
/// `classify_decision`.
fn has_replacement_cue(text: &str) -> bool {
    const CUES: &[&str] = &[
        "instead",
        "replace",
        "revert",
        "switch",
        "drop ",
        "remove ",
        "no, ",
        "actually ",
    ];
    let lower = text.to_lowercase();
    CUES.iter().any(|cue| lower.contains(cue))
}

/// Words that carry no requirement on their own: articles, prepositions,
/// copulas and the directive verbs/cues themselves. A shared word from this
/// set is never evidence that two decisions address the same requirement.
fn is_stop_word(word: &str) -> bool {
    const STOP: &[&str] = &[
        "the",
        "a",
        "an",
        "to",
        "for",
        "with",
        "of",
        "in",
        "on",
        "at",
        "by",
        "and",
        "or",
        "but",
        "is",
        "are",
        "be",
        "was",
        "were",
        "it",
        "its",
        "this",
        "that",
        "these",
        "those",
        "from",
        "into",
        "as",
        "use",
        "using",
        "used",
        "switch",
        "switching",
        "revert",
        "reverting",
        "drop",
        "dropping",
        "remove",
        "removing",
        "replace",
        "replacing",
        "prefer",
        "adopt",
        "instead",
        "actually",
        "please",
        "should",
        "must",
        "will",
        "would",
        "can",
        "could",
        "let",
        "lets",
        "we",
        "i",
        "you",
        "now",
        "then",
        "also",
        "not",
        "no",
        "yes",
        "all",
        "any",
    ];
    STOP.contains(&word)
}

/// Lowercased content words (length >= 3, not stop words), deduplicated.
/// Used to test whether the replacing message actually names the *subject*
/// of an older decision rather than merely co-mentioning its file.
fn content_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .to_lowercase()
        })
        .filter(|word| word.len() >= 3 && !is_stop_word(word))
        .collect();
    words.sort();
    words.dedup();
    words
}

/// True when the incoming message proves it addresses the same requirement
/// as the older decision, rather than just naming the same file (F02).
///
/// A shared file path is a *relevance* signal: it says two decisions are
/// about the same resource, not that they are about the same requirement.
/// "use AuthService.rs with a 5-second timeout" and "replace plain-text
/// logging in AuthService.rs with structured logging" share the file but
/// withdraw nothing from each other — the cue `replace` is scoped to
/// `plain-text logging`, not to the file. Proof needs one of:
///
/// - a **whole-entity withdrawal**: the message withdraws the shared entity
///   itself ("use X instead", "drop X", "switch to Y"). Here the cue's
///   object *is* the decision, so naming the same file is enough.
/// - the two share a non-file content word — a concrete requirement noun
///   such as `timeout`, `logging` or `toml` — which is the dimension being
///   replaced, or
/// - the message quotes a distinctive phrase of the older decision.
///
/// File-path tokens are excluded from the word comparison so path equality
/// alone can never satisfy the shared-word branch.
fn names_the_same_requirement(content: &str, older: &ContextItem) -> bool {
    names_the_same_requirement_in(content, &older.content)
}

/// [`names_the_same_requirement`] over a plain older text, so the external
/// map — which only keeps a stored summary — can apply the same proof.
fn names_the_same_requirement_in(content: &str, older_text: &str) -> bool {
    // CTX-2: retaining/negating wording ("do not replace …", "keep the
    // timeout") is the opposite of a replacement declaration. It wins over
    // every cue below — ambiguity coexists.
    if has_retention_protection(content) {
        return false;
    }
    // A verbatim run of several words is the strongest available proof:
    // "replace the AuthService.rs 5-second timeout with …" quotes the line
    // it withdraws. CTX-2: the run must contain at least one CONTENT word —
    // a template opening like "use AuthService.rs with" is shared by every
    // decision about that file and quotes nothing. And when the message
    // contains the older line VERBATIM, it is a restatement/superset of the
    // old decision, not a quotation for replacement — the object-naming
    // rule below still decides.
    if !contains_verbatim(older_text, content) && shares_content_run(content, older_text, 3) {
        return true;
    }
    // CTX-2/E02: a replacement declaration must NAME what it replaces, and
    // the named object must be part of the older decision. A bare cue plus
    // a shared file (or any shared word anywhere in the message) is no
    // longer accepted — "… with structured logging instead of plain-text
    // logging" names logging as the replaced object, so it cannot withdraw
    // the same file's 5-second-timeout requirement, and restating a
    // requirement is not revoking it.
    if names_replaced_object(content, older_text) {
        return true;
    }
    // Direct-object withdrawal verbs keep their whole-entity rule: "drop
    // AuthService.rs" — the entity itself is the object being retracted.
    has_whole_entity_cue(content, older_text)
}

/// Words that mark retention or negation: a message carrying one of these
/// contradicts a replacement declaration and never supersedes anything.
fn has_retention_protection(content: &str) -> bool {
    let tokens: Vec<String> = content
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .to_lowercase()
        })
        .collect();
    for token in &tokens {
        if matches!(
            token.as_str(),
            "keep"
                | "keeps"
                | "keeping"
                | "retain"
                | "retains"
                | "retaining"
                | "preserve"
                | "preserves"
                | "preserving"
                | "remains"
                | "remain"
                | "still"
                | "never"
                | "dont"
                | "not"
        ) {
            return true;
        }
    }
    tokens.windows(2).any(|pair| pair == ["do", "not"])
}

/// True when a replacement cue's OBJECT names part of the older decision.
///
/// The object positions are explainable surface patterns:
/// - `replace <obj> (with …)?` — the direct object of `replace`;
/// - `instead of <obj>` / `rather than <obj>` — the contrast phrase;
/// - `replaces|replaced|replacing <obj>`.
///
/// The object phrase (up to four content words) is compared against the
/// older decision's content words and entities; file-path words are
/// excluded on both sides, so path equality alone never satisfies it.
fn names_replaced_object(content: &str, older_text: &str) -> bool {
    let mut older_words: Vec<String> = content_words(older_text)
        .into_iter()
        .filter(|word| !is_file_path_entity(word))
        .collect();
    for entity in extract_entities(older_text) {
        let entity = entity.to_lowercase();
        if !is_file_path_entity(&entity) {
            older_words.push(entity);
        }
    }
    if older_words.is_empty() {
        return false;
    }
    let tokens: Vec<String> = content
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .to_lowercase()
        })
        .collect();
    // CTX-2 残余（R3-03）：替换宾语的*每一个*内容词都必须出现在旧决策里。
    // 只共享一个需求维度词（"timeout logging" vs "5-second timeout"）是
    // 相关性信号，不是撤销证明——命名一条要求意味着命名它的全部内容词。
    // 不确定时保守共存，实体/词语继续服务于检索与相关性。
    let object_names_older = |object: &[String]| -> bool {
        !object.is_empty()
            && object
                .iter()
                .all(|word| !word.is_empty() && older_words.iter().any(|prior| prior == word))
    };
    let mut index = 0;
    while index < tokens.len() {
        // `instead of <obj>` / `rather than <obj>`: the contrast object.
        let contrast = (tokens[index] == "instead"
            && index + 1 < tokens.len()
            && (tokens[index + 1] == "of" || tokens[index + 1] == "than"))
            || (tokens[index] == "rather"
                && index + 1 < tokens.len()
                && tokens[index + 1] == "than");
        if contrast {
            let object: Vec<String> = tokens[index + 2..]
                .iter()
                .take(4)
                .filter(|word| !is_stop_word(word) && !is_file_path_entity(word))
                .cloned()
                .collect();
            if object_names_older(&object) {
                return true;
            }
            index += 2;
            continue;
        }
        // `replace <obj> (with …)`: the direct object runs until `with`.
        if matches!(
            tokens[index].as_str(),
            "replace" | "replaces" | "replaced" | "replacing"
        ) {
            let mut object: Vec<String> = Vec::new();
            for token in tokens.iter().skip(index + 1).take(6) {
                if token == "with" {
                    break;
                }
                // File-path words are excluded on both sides (the doc above):
                // a location phrase inside the object ("replace the 5-second
                // timeout in AuthService.rs with …") is not requirement
                // content, and it must not block the all-word proof below.
                if !is_stop_word(token) && !is_file_path_entity(token) {
                    object.push(token.clone());
                }
                if object.len() >= 4 {
                    break;
                }
            }
            if object_names_older(&object) {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// True when `content` withdraws the shared entity itself rather than a
/// scoped part of it.
///
/// The distinguishing feature is the cue verb's **direct object**:
///
/// - `drop AuthService.rs for the cache layer` — the object of the verb is
///   the entity itself, so the decision is retracted.
/// - `replace plain-text logging in AuthService.rs with structured logging`
///   — the object of `replace` is `plain-text logging`; the entity appears
///   only as a prepositional *location* (`in AuthService.rs`). Withdrawing
///   one dimension of a file is not withdrawing the file's other decisions.
///
/// CTX-2/E02: the former `instead`/`revert` shortcut is gone. `instead`
/// often introduces a *contrast phrase* ("… instead of plain-text
/// logging") whose object is a scoped dimension, not the whole entity;
/// treating it as a whole-entity withdrawal revoked unrelated requirements
/// on the same file. Contrast phrases are handled by
/// [`names_replaced_object`], which requires the named object to actually
/// match the older decision.
fn has_whole_entity_cue(content: &str, older_text: &str) -> bool {
    let older_entities = extract_entities(older_text);
    if older_entities.is_empty() {
        return false;
    }
    let tokens: Vec<String> = content
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                .to_lowercase()
        })
        .collect();
    let shared = |token: &str| -> bool {
        older_entities
            .iter()
            .any(|entity| entity.to_lowercase() == token)
    };
    // `drop` / `remove` / `switch`(to) / `use` take a direct object: the
    // entity must be that object, not a later prepositional location.
    const DIRECT_OBJECT_CUES: &[&str] = &["drop", "remove", "switch", "discard", "abandon"];
    for (index, token) in tokens.iter().enumerate() {
        if !DIRECT_OBJECT_CUES.contains(&token.as_str()) {
            continue;
        }
        // Skip determiners/adjectives that may precede the object.
        for candidate in tokens.iter().skip(index + 1).take(3) {
            if matches!(
                candidate.as_str(),
                "to" | "the" | "a" | "an" | "our" | "this"
            ) {
                continue;
            }
            if shared(candidate) {
                return true;
            }
            // The first real object noun decides: if it is not the shared
            // entity, the withdrawal is scoped elsewhere.
            break;
        }
    }
    false
}

/// Whether `left` and `right` share a verbatim run of at least `run` words
/// (case-insensitive, punctuation-insensitive). Cheap and explainable: it
/// is the "quotes the line it replaces" signal.
/// True when `needle`'s full normalized word sequence appears verbatim
/// (contiguously) inside `haystack` — a restatement or superset.
fn contains_verbatim(needle: &str, haystack: &str) -> bool {
    let normalize = |text: &str| -> Vec<String> {
        text.split_whitespace()
            .map(|word| {
                word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                    .to_lowercase()
            })
            .filter(|word| !word.is_empty())
            .collect()
    };
    let needle_words = normalize(needle);
    let haystack_words = normalize(haystack);
    needle_words.len() >= 3
        && haystack_words
            .windows(needle_words.len())
            .any(|window| window == needle_words.as_slice())
}

/// [`shares_verbatim_run`], additionally requiring the matched run to
/// carry at least one non-stop, non-path content word: a run of template
/// words ("use <file> with") names no requirement.
fn shares_content_run(left: &str, right: &str, run: usize) -> bool {
    let normalize = |text: &str| -> Vec<String> {
        text.split_whitespace()
            .map(|word| {
                word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                    .to_lowercase()
            })
            .filter(|word| !word.is_empty())
            .collect()
    };
    let left_words = normalize(left);
    let right_words = normalize(right);
    if left_words.len() < run || right_words.len() < run {
        return false;
    }
    left_words.windows(run).any(|window| {
        right_words.windows(run).any(|other| {
            other == window
                && window
                    .iter()
                    .any(|word| !is_stop_word(word) && !is_file_path_entity(word))
        })
    })
}

/// True when the item is permanently excluded from model requests: a
/// superseded decision or a verified-fixed error, whatever its score. The
/// semantic state is authoritative; the legacy lifecycle labels are only
/// honored for pre-split checkpoints until restore migrates them.
pub(crate) fn is_excluded(item: &ContextItem) -> bool {
    item.semantic.is_dead()
        || item.tags.iter().any(|tag| {
            tag.is_lifecycle(LifecycleLabel::Superseded)
                || tag.is_lifecycle(LifecycleLabel::VerifiedFixed)
        })
}

/// Queue supersession of an earlier decision, but only when the incoming
/// decision *proves* it replaces that specific line (F15, F02): the same
/// task context, an explicit replacement cue in the message, and — beyond
/// an exact path/symbol identity (`entities_match_exact`) — a shared
/// *requirement*, not merely a shared file. Entity overlap is a relevance
/// signal, not proof: two compatible decisions about one file ("use
/// AuthService.rs with a 5-second timeout", "replace plain-text logging in
/// AuthService.rs with structured logging") share the key but withdraw
/// nothing, so both stay live, and a decision from another task never
/// finalizes one from this task even on a full key match.
///
/// F02 narrowed the last same-task hole: the replacement cue applies to the
/// whole message, so a message that changes one dimension of a file
/// (`replace … logging …`) used to finalize every other decision on that
/// file (`… 5-second timeout …`). Finalization therefore additionally
/// requires [`names_the_same_requirement`] — the message must quote the
/// older line or share a concrete requirement word with it. When no proof
/// exists nothing is queued — the older decision keeps its state and decays
/// through the ordinary relevance scorer; attention is deliberately not
/// demoted here, because that would be a second unproven judgment.
///
/// `by_id` is the new decision's id: it excludes the new item itself and
/// becomes the `by` of the Superseded semantic state. `task_id` is the new
/// decision's task context (session-level messages carry `None`; two
/// session-level decisions share that one context). The scan covers the
/// heap, the warm buffer and the external map, so a proven replacement
/// supersedes the earlier decision wherever its body currently sits.
pub(crate) fn queue_decision_supersessions(
    state: &mut State,
    content: &str,
    reason_prefix: &str,
    by_id: ContextItemId,
    task_id: Option<agent_contracts::TaskId>,
) {
    let entities = extract_entities(content);
    if entities.is_empty() || !has_replacement_cue(content) {
        return;
    }
    let is_decision = |item: &ContextItem| {
        item.kind == ContextKind::Decision
            || item.tags.iter().any(|tag| tag.is_core(CoreLabel::Decision))
    };
    let matches = |item: &ContextItem| -> bool {
        item.id != by_id
            && is_decision(item)
            && !item.semantic.is_dead()
            && !is_excluded(item)
            && item.task_id == task_id
            && entities_match_exact(&entities, &item.entities)
            && names_the_same_requirement(content, item)
    };
    for item in &mut state.items {
        if !matches(item) {
            continue;
        }
        let snippet: String = item.content.chars().take(60).collect();
        state
            .pending_supersessions
            .push((item.id, by_id, format!("{reason_prefix}: '{snippet}'")));
    }
    for item in &mut state.eviction_buffer {
        if !matches(item) {
            continue;
        }
        let snippet: String = item.content.chars().take(60).collect();
        state
            .pending_supersessions
            .push((item.id, by_id, format!("{reason_prefix}: '{snippet}'")));
    }
    // CTX-6: the externalize-retry list owns full live bodies while the
    // store is down — a proven replacement supersedes that decision too,
    // wherever its body currently sits.
    for item in &mut state.pending_externalize_retry {
        if !matches(item) {
            continue;
        }
        let snippet: String = item.content.chars().take(60).collect();
        state
            .pending_supersessions
            .push((item.id, by_id, format!("{reason_prefix}: '{snippet}'")));
    }
    for entry in &state.external {
        let decision = entry.kind == ContextKind::Decision
            || entry
                .tags
                .iter()
                .any(|tag| tag.is_core(CoreLabel::Decision));
        if entry.item_id == by_id
            || !decision
            || entry.semantic.is_dead()
            || entry.task_id != task_id
            || !entities_match_exact(&entities, &entry.entities)
            // F02: the stored body is only a summary, but it still has to
            // name the same requirement — a shared file never suffices.
            || !names_the_same_requirement_in(content, &entry.context_ref.summary)
        {
            continue;
        }
        state.pending_supersessions.push((
            entry.item_id,
            by_id,
            format!("{reason_prefix}: stored decision"),
        ));
    }
}

/// Queue verification for live, unverified error items recorded by the
/// same task and immutable host probe that now succeeded. The probe binds
/// the complete recipe definition, revision and declared coverage. M17-B3/F08: entity
/// overlap alone is correlation, not proof — a successful run of one
/// recipe (or of any plain tool) must never finalize errors it did not
/// probe, so errors without a matching recipe association stay live.
/// Also covers the warm buffer and the external map: an error that left
/// Resident is still the same error and still gets verified by a later
/// success of its own recipe.
pub(crate) fn queue_error_verifications(
    state: &mut State,
    reason: &str,
    by_id: ContextItemId,
    task_id: Option<agent_contracts::TaskId>,
    probe: Option<&agent_contracts::VerificationProbe>,
) {
    let (Some(task_id), Some(probe)) = (task_id, probe) else {
        // A success without a recipe identity cannot prove anything about
        // any recorded fault.
        return;
    };
    let matches = |item: &ContextItem| -> bool {
        item.kind == ContextKind::Error
            && !item.semantic.is_dead()
            && !is_excluded(item)
            && item.task_id == Some(task_id)
            && item.verify_recipe.as_ref() == Some(probe)
    };
    for item in &mut state.items {
        if matches(item) {
            state
                .pending_verifications
                .push((item.id, by_id, reason.to_string()));
        }
    }
    for item in &mut state.eviction_buffer {
        if matches(item) {
            state
                .pending_verifications
                .push((item.id, by_id, reason.to_string()));
        }
    }
    // CTX-6: a spilled (store-unavailable) error is still the same error and
    // still gets verified by a later success of its own recipe.
    for item in &mut state.pending_externalize_retry {
        if matches(item) {
            state
                .pending_verifications
                .push((item.id, by_id, reason.to_string()));
        }
    }
    for entry in &state.external {
        if entry.kind == ContextKind::Error
            && entry.item_id != by_id
            && entry.semantic.is_live()
            && entry.task_id == Some(task_id)
            && entry.verify_recipe.as_ref() == Some(probe)
        {
            state
                .pending_verifications
                .push((entry.item_id, by_id, reason.to_string()));
        }
    }
}

/// 同一文件路径的更新 **文件正文**（`fs.read` / unsourced replay header）
/// 只有在能证明覆盖时才覆盖旧正文：内容修订不同（明确的过期边界），或
/// 同一修订下新正文完整包含旧正文（新窗口覆盖旧窗口/全文重读）。同版本
/// 的非重叠片段是互补证据，不得仅因路径相同被 supersede；缺修订或无法
/// 证明覆盖时保守保留。带 `metadata.path` 的 `shell.exec` 只把路径写入
/// 身份索引，不是文件正文，不得互相 supersede。按结构化路径精确匹配，
/// 回退到正文首行；不用实体子串（`Session::start` 会把三个文件缠在一起）。
pub(crate) fn queue_file_body_supersessions(state: &mut State, new_item: &ContextItem) {
    if !is_file_body_observation(new_item) {
        return;
    }
    let Some(path) = observation_file_path(new_item).map(str::to_owned) else {
        return;
    };
    let by_id = new_item.id;
    let is_same_file = |item: &ContextItem| -> bool {
        item.id != by_id
            && item.kind == ContextKind::ToolObservation
            && !item.semantic.is_dead()
            && !is_excluded(item)
            && is_file_body_observation(item)
            && observation_file_path(item) == Some(path.as_str())
    };
    let supersedes = |item: &ContextItem| -> bool { supersedes_file_body(new_item, item) };
    let reason_for = |item: &ContextItem| -> String {
        if supersedes_stale_revision(new_item, item) {
            format!("superseded by a newer revision of {path}")
        } else {
            format!("superseded by a covering re-read of {path}")
        }
    };
    for item in &mut state.items {
        if is_same_file(item) && supersedes(item) {
            let reason = reason_for(item);
            state.pending_supersessions.push((item.id, by_id, reason));
        }
    }
    for item in &mut state.eviction_buffer {
        if is_same_file(item) && supersedes(item) {
            let reason = reason_for(item);
            state.pending_supersessions.push((item.id, by_id, reason));
        }
    }
    // CTX-6: a pending file body is a full in-memory body — the same
    // revision/coverage proof applies to it as to any resident body.
    for item in &mut state.pending_externalize_retry {
        if is_same_file(item) && supersedes(item) {
            let reason = reason_for(item);
            state.pending_supersessions.push((item.id, by_id, reason));
        }
    }
    for entry in &state.external {
        if entry.item_id == by_id
            || entry.kind != ContextKind::ToolObservation
            || entry.semantic.is_dead()
            || !is_file_body_entry(entry)
        {
            continue;
        }
        if entry.file_path.as_deref() == Some(path.as_str())
            || entry.entities.iter().any(|entity| entity == &path)
        {
            // Stored entries carry no content revision, so staleness cannot
            // be proven and the blob body is not loaded for a containment
            // check: conservatively keep the stored fragment. It stays
            // retrievable; a same-revision stored body is still valid, and
            // an admitted one is compared like any resident body.
        }
    }
}

/// A newer read supersedes an older body of the same file only when it
/// proves coverage: a different content revision (explicit stale boundary)
/// or a same-revision window that covers the older fragment.
fn supersedes_file_body(new_item: &ContextItem, old: &ContextItem) -> bool {
    supersedes_stale_revision(new_item, old) || supersedes_same_revision_body(new_item, old)
}

fn supersedes_stale_revision(new_item: &ContextItem, old: &ContextItem) -> bool {
    match (&old.file_revision, &new_item.file_revision) {
        (Some(old_rev), Some(new_rev)) => old_rev != new_rev,
        _ => false,
    }
}

/// Same content revision: the newer body supersedes the older one only when
/// it proves coverage. A trusted line window that contains the older window
/// is enough — but only from a body the engine did not clip: the declared
/// range is the tool-reported interval, and a clipped body retains only a
/// prefix of it, so its range is not a proof of retained coverage
/// (RANGE-PARTIAL). If either side lacks a range, only a literal body
/// containment of unclipped text is proof. Disjoint windows coexist; unknown
/// or clipped bodies are never proven and are kept.
fn supersedes_same_revision_body(new_item: &ContextItem, old: &ContextItem) -> bool {
    match (&old.file_revision, &new_item.file_revision) {
        (Some(old_rev), Some(new_rev)) if old_rev == new_rev => {
            // A clipped new body skips the range proof and falls through to
            // the literal guards below, which refuse clipped bodies:
            // conservative coexistence.
            if let (Some((new_start, new_end)), Some((old_start, old_end))) =
                (file_line_range(new_item), file_line_range(old))
                && !crate::item::content_was_clipped(&new_item.content)
            {
                return new_start <= old_start && new_end >= old_end;
            }
            if crate::item::content_was_clipped(&old.content)
                || crate::item::content_was_clipped(&new_item.content)
            {
                return false;
            }
            !old.content.is_empty() && new_item.content.contains(&old.content)
        }
        _ => false,
    }
}

fn file_line_range(item: &ContextItem) -> Option<(u32, u32)> {
    let start = item.file_start_line?;
    let end = item.file_end_line?;
    (start >= 1 && end >= start).then_some((start, end))
}

/// Coalesce identical failures within one task and producer/check identity.
/// Resident and warm bodies can prove exact equality. Lossy external
/// descriptors cannot; their faults remain live until verified or closed.
pub(crate) fn queue_error_recurrence(state: &mut State, new_item: &ContextItem, round: u64) {
    // Same-file entity overlap is not a fault identity. Deduplicate only
    // an identical observation from the same task, producer and check.
    let matches = |item: &ContextItem| {
        item.id != new_item.id
            && item.kind == ContextKind::Error
            && !item.semantic.is_dead()
            && !is_excluded(item)
            && item.task_id == new_item.task_id
            && item.source == new_item.source
            && item.verify_recipe == new_item.verify_recipe
            && item.content == new_item.content
    };
    for item in state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        // CTX-6: a pending error body is identical-fault provable too.
        .chain(state.pending_externalize_retry.iter())
        .filter(|item| matches(item))
    {
        state.pending_supersessions.push((
            item.id,
            new_item.id,
            format!("recurring failure supersedes earlier error (round {round}, identical fault)"),
        ));
    }
    // External summaries are lossy. They cannot prove identical fault
    // content; keep them until a matching trusted probe or explicit closure.
}

/// Apply queued supersession intents as observable state changes: the older
/// decision is archived and its semantic state becomes
/// `Superseded { by }` — terminal, never resurrected by Context GC.
///
/// The target may live in any body location: the resident heap, the warm
/// reversible buffer, or the external map. Lifecycle authority must not
/// depend on where the body currently sits: a decision that was
/// evicted and externalized is still the same decision and still gets
/// superseded.
pub(crate) fn drain_supersessions(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let supersessions = std::mem::take(&mut state.pending_supersessions);
    for (item_id, by_id, reason) in supersessions {
        if let Some(transition) = apply_terminal_semantic(
            state,
            item_id,
            SemanticState::Superseded { by: Some(by_id) },
            &reason,
            turn,
        ) {
            transitions.push(transition);
        }
    }
    transitions
}

/// Apply queued verification intents as observable state changes: the error
/// is archived and its semantic state becomes `VerifiedFixed { by }` — also
/// independent of body location, so an error that left Resident still gets
/// verified when a later successful result fixes it.
pub(crate) fn drain_verifications(state: &mut State, turn: u64) -> Vec<ContextStateTransition> {
    let mut transitions = Vec::new();
    let verifications = std::mem::take(&mut state.pending_verifications);
    for (item_id, by_id, reason) in verifications {
        // Queues are checkpointed too. Old or corrupted pending intents must
        // not bypass the new probe checks when resumed after an upgrade.
        if !has_matching_verification_evidence(state, item_id, by_id) {
            continue;
        }
        if let Some(transition) = apply_terminal_semantic(
            state,
            item_id,
            SemanticState::VerifiedFixed { by: Some(by_id) },
            &reason,
            turn,
        ) {
            transitions.push(transition);
        }
    }
    transitions
}

fn has_matching_verification_evidence(
    state: &State,
    target: ContextItemId,
    by: ContextItemId,
) -> bool {
    fn identity(
        state: &State,
        id: ContextItemId,
    ) -> Option<(
        agent_contracts::TaskId,
        &agent_contracts::VerificationProbe,
        ContextKind,
    )> {
        if let Some(item) = state
            .items
            .indexes()
            .get(id)
            .and_then(|slot| state.items.get(slot))
            .or_else(|| state.eviction_buffer.iter().find(|item| item.id == id))
            // CTX-6: a pending body proves its recipe identity the same way.
            .or_else(|| {
                state
                    .pending_externalize_retry
                    .iter()
                    .find(|item| item.id == id)
            })
        {
            return Some((item.task_id?, item.verify_recipe.as_ref()?, item.kind));
        }
        let entry = state.external.get(id)?;
        Some((entry.task_id?, entry.verify_recipe.as_ref()?, entry.kind))
    }
    match (identity(state, target), identity(state, by)) {
        (
            Some((task, probe, ContextKind::Error)),
            Some((by_task, by_probe, ContextKind::ToolObservation)),
        ) => task == by_task && probe == by_probe,
        _ => false,
    }
}

/// Apply one terminal semantic transition to an item in whatever body
/// location it currently occupies: resident heap, warm buffer, or external
/// map. Semantic transitions are monotonic — a dead target stays dead — and
/// the change is recorded as an observable transition. `None` when the item
/// is unknown or already terminal.
fn apply_terminal_semantic(
    state: &mut State,
    item_id: ContextItemId,
    terminal: SemanticState,
    reason: &str,
    turn: u64,
) -> Option<ContextStateTransition> {
    // Resident heap.
    if let Some(item) = state.items.iter_mut().find(|item| item.id == item_id) {
        if item.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from: item.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    // Warm reversible buffer.
    if let Some(item) = state
        .eviction_buffer
        .iter_mut()
        .find(|item| item.id == item_id)
    {
        if item.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from: item.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    // CTX-6: the externalize-retry list owns full live bodies while the
    // store is down — a queued terminal transition lands there exactly like
    // on the heap or the warm buffer. The item stays in the retry list; the
    // store write that eventually succeeds carries the terminal semantics.
    if let Some(item) = state
        .pending_externalize_retry
        .iter_mut()
        .find(|item| item.id == item_id)
    {
        if item.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: item.id,
            kind: item.kind,
            scope: item.scope,
            from: item.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        item.attention = AttentionState::Archived;
        item.relevance = 0.0;
        item.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    // Stored (Cold / External): metadata only — the entry keeps its terminal
    // state in the map and is no longer retrievable.
    if let Some(entry) = state.external.get_mut(item_id) {
        if entry.semantic.is_dead() {
            return None;
        }
        let transition = ContextStateTransition {
            item_id: entry.item_id,
            kind: entry.kind,
            scope: entry.scope,
            from: entry.attention,
            to: AttentionState::Archived,
            turn,
            reason: reason.to_string(),
        };
        entry.attention = AttentionState::Archived;
        entry.semantic = terminal;
        state.mark_catalog(item_id);
        return Some(transition);
    }
    None
}
