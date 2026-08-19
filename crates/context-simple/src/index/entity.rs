use std::collections::{HashMap, HashSet};

use agent_contracts::{ContextItem, ContextItemId, ContextKind, ExternalizedContext, TaskId};

/// Cap for the hot-entity set, matching `extract_entities`'s per-text cap.
pub(crate) const MAX_HOT_ENTITIES: usize = 24;

/// 当前任务里保留「每个文件最新正文」的路径数上限。这是驻留界，不是
/// 打分阈值：循环重构只动少数文件，整堆日志仍按 ephemeral 消费淘汰。
pub(crate) const MAX_RECENT_FILE_BODIES: usize = 8;

/// Entity extraction shared across the crate (hot-set maintenance, dependency
/// linking, supersession, scoring). "Entity" is a cheap, explicit signature: a
/// whitespace-separated token of length >= 3 that carries a path/name/case
/// marker (`.`, `/`, `::`, `_` or an uppercase letter). Surrounding
/// punctuation (including `?` / `!`) is stripped so a user question about
/// `AuthService.rs` still exact-matches the observation entity. Sorted,
/// deduplicated, bounded to 24 per text.
pub(crate) fn extract_entities(text: &str) -> Vec<String> {
    let mut entities: Vec<String> = text
        .split_whitespace()
        .map(|s| s.trim_matches(|c: char| ",;()[]{}<>\"'`?!".contains(c)))
        .filter(|s| {
            s.len() >= 3
                && (s.contains('.')
                    || s.contains('/')
                    || s.contains("::")
                    || s.contains('_')
                    || s.chars().any(|c| c.is_ascii_uppercase()))
        })
        .take(24)
        .map(ToOwned::to_owned)
        .collect();
    entities.sort();
    entities.dedup();
    entities
}

/// Whether any entity of `left` overlaps any of `right`, with a substring
/// tolerance so `AuthService.rs` matches `src/auth/AuthService.rs`.
/// Search and scoring affinity keep this; auto-reactivation does not.
pub(crate) fn entities_match(left: &[String], right: &[String]) -> bool {
    left.iter().any(|entity| {
        right
            .iter()
            .any(|prior| prior.contains(entity) || entity.contains(prior))
    })
}

/// Slash-normalized equality. Auto-reactivation and hot GC roots use this
/// so `src/foo.rs` does not resurrect substring cousins.
pub(crate) fn entities_match_exact(left: &[String], right: &[String]) -> bool {
    left.iter().any(|entity| {
        let entity = agent_contracts::normalize_resource_path(entity);
        right
            .iter()
            .any(|prior| agent_contracts::normalize_resource_path(prior) == entity)
    })
}

/// 看起来像文件路径：含 `/` `\`，或 `stem.ext` 短扩展名；`Module::path`
/// 不算（那是符号，不是文件正文）。
pub(crate) fn is_file_path_entity(entity: &str) -> bool {
    if entity.contains("::") {
        return false;
    }
    if entity.contains('/') || entity.contains('\\') {
        return true;
    }
    let Some((stem, ext)) = entity.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=8).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `fs.read` / replay `read_snippet` 的正文：首行是单独的路径（可带尾部
/// `:`）。「tests passed in AuthService.rs」这种日志只是提到文件，不是
/// 文件正文，必须返回 `None`，否则成功日志会被当成 latest-file 根留下。
pub(crate) fn primary_file_path(content: &str) -> Option<&str> {
    let head = content.lines().next()?.trim();
    let path = head.strip_suffix(':').unwrap_or(head).trim();
    if path.is_empty() || path.chars().any(char::is_whitespace) {
        return None;
    }
    if is_file_path_entity(path) {
        Some(path)
    } else {
        None
    }
}

/// 文件正文观察的工作区路径：优先结构化 `file_path`，回退到正文首行
/// （replay `path:\nbody` 夹具）。live `fs.read` 正文是 `     1 | …`，
/// 没有路径，必须走字段。
pub(crate) fn observation_file_path(item: &ContextItem) -> Option<&str> {
    observation_file_path_parts(item.file_path.as_deref(), &item.content)
}

pub(crate) fn observation_file_path_entry(entry: &ExternalizedContext) -> Option<&str> {
    observation_file_path_parts(entry.file_path.as_deref(), &entry.context_ref.summary)
}

/// 把路径写入实体索引，供 catalog search 按路径命中。
pub(crate) fn index_file_path(entities: &mut Vec<String>, path: &str) {
    let path = agent_contracts::normalize_resource_path(path);
    if path.is_empty() || entities.iter().any(|existing| existing == &path) {
        return;
    }
    entities.insert(0, path);
    entities.truncate(MAX_HOT_ENTITIES);
}

/// 当前任务里每个最近文件路径的最新成功观察。同一路径的旧正文会被更新的
/// 读覆盖；超过 [`MAX_RECENT_FILE_BODIES`] 的更早路径不入选。没有焦点时
/// 返回空集——文件正文根只服务活跃任务，避免跨任务污染。
pub(crate) fn latest_file_body_ids<'a>(
    items: impl IntoIterator<Item = &'a ContextItem>,
    active_task: Option<TaskId>,
    max_bodies: usize,
    lease_turns: u64,
    current_turn: u64,
) -> HashSet<ContextItemId> {
    let Some(task) = active_task else {
        return HashSet::new();
    };
    let cap = max_bodies.max(1);
    let mut latest: HashMap<&str, (u64, u64, ContextItemId)> = HashMap::new();
    for item in items {
        if item.task_id != Some(task)
            || item.kind != ContextKind::ToolObservation
            || !item.semantic.is_live()
            || !is_file_body_observation(item)
        {
            continue;
        }
        if lease_turns > 0 && current_turn.saturating_sub(item.created_turn) >= lease_turns {
            continue;
        }
        let Some(path) = observation_file_path(item) else {
            continue;
        };
        match latest.get(path) {
            Some((tick, _, _)) if *tick >= item.created_tick => {}
            _ => {
                latest.insert(path, (item.created_tick, item.created_turn, item.id));
            }
        }
    }
    let mut ranked: Vec<(u64, ContextItemId)> = latest
        .into_values()
        .map(|(tick, _, id)| (tick, id))
        .collect();
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    ranked.into_iter().take(cap).map(|(_, id)| id).collect()
}

/// File body (`fs.read` or unsourced replay `path:\nbody`), not a tool that
/// merely stamped a resource path onto a log.
pub(crate) fn is_file_body_observation(item: &ContextItem) -> bool {
    is_file_body(
        item.source.as_deref(),
        item.file_path.as_deref(),
        &item.content,
    )
}

pub(crate) fn is_file_body_entry(entry: &ExternalizedContext) -> bool {
    is_file_body(
        entry.source.as_deref(),
        entry.file_path.as_deref(),
        &entry.context_ref.summary,
    )
}

fn is_file_body(source: Option<&str>, file_path: Option<&str>, content: &str) -> bool {
    match source {
        Some("tool:fs.read") => observation_file_path_parts(file_path, content).is_some(),
        Some(_) => false,
        None => primary_file_path(content).is_some(),
    }
}

fn observation_file_path_parts<'a>(
    file_path: Option<&'a str>,
    content: &'a str,
) -> Option<&'a str> {
    if let Some(path) = file_path {
        let path = path.trim();
        if !path.is_empty() && is_file_path_entity(path) {
            return Some(path);
        }
    }
    primary_file_path(content)
}

/// Merge tool-touched entities into the hot set: most recent first,
/// deduplicated, bounded.
pub(crate) fn merge_hot_entities(hot: &mut Vec<String>, entities: Vec<String>) {
    for entity in entities {
        if let Some(position) = hot.iter().position(|existing| existing == &entity) {
            hot.remove(position);
        }
        hot.insert(0, entity);
    }
    hot.truncate(MAX_HOT_ENTITIES);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_entities_detects_paths_and_case_signatures_only() {
        assert_eq!(
            extract_entities("fix AuthService.rs and check src/auth/mod.rs"),
            vec!["AuthService.rs".to_string(), "src/auth/mod.rs".to_string()]
        );
        assert_eq!(
            extract_entities("what did we change in AuthService.rs?"),
            vec!["AuthService.rs".to_string()]
        );
        assert!(
            extract_entities("all lowercase words without markers").is_empty(),
            "plain words must not be entities"
        );
    }

    #[test]
    fn extract_entities_is_bounded_and_deduplicated() {
        let text = (0..40)
            .map(|i| format!("File{i}.rs"))
            .collect::<Vec<_>>()
            .join(" ");
        let entities = extract_entities(&text);
        assert_eq!(entities.len(), 24, "capped at 24");
        let mut dedup = entities.clone();
        dedup.dedup();
        assert_eq!(entities, dedup, "sorted + deduplicated");
    }

    #[test]
    fn entities_match_uses_substring_tolerance() {
        assert!(entities_match(
            &["AuthService.rs".to_string()],
            &["src/auth/AuthService.rs".to_string()]
        ));
        assert!(!entities_match(
            &["AuthService.rs".to_string()],
            &["CacheStore.rs".to_string()]
        ));
    }

    #[test]
    fn entities_match_exact_requires_slash_normalized_identity() {
        assert!(entities_match_exact(
            &["src/foo.rs".to_string()],
            &["src\\foo.rs".to_string()]
        ));
        assert!(
            !entities_match_exact(
                &["AuthService.rs".to_string()],
                &["src/auth/AuthService.rs".to_string()]
            ),
            "substring cousins must not auto-reactivate"
        );
    }

    #[test]
    fn merge_hot_entities_is_newest_first_and_bounded() {
        let mut hot = Vec::new();
        merge_hot_entities(&mut hot, vec!["a.rs".into(), "b.rs".into()]);
        assert_eq!(hot, vec!["b.rs".to_string(), "a.rs".to_string()]);
        merge_hot_entities(&mut hot, vec!["a.rs".into()]);
        assert_eq!(hot, vec!["a.rs".to_string(), "b.rs".to_string()]);

        let many = (0..40).map(|i| format!("f{i}.rs")).collect();
        merge_hot_entities(&mut hot, many);
        assert_eq!(hot.len(), MAX_HOT_ENTITIES);
    }

    #[test]
    fn primary_file_path_accepts_read_snippet_and_rejects_logs() {
        assert_eq!(
            primary_file_path("src/auth/login.rs:\nfn handle_21() {}"),
            Some("src/auth/login.rs")
        );
        assert_eq!(
            primary_file_path("AuthService.rs\nfn run() {}"),
            Some("AuthService.rs")
        );
        assert_eq!(
            primary_file_path("tests passed in AuthService.rs"),
            None,
            "a log that mentions a file is not the file body"
        );
        assert_eq!(
            primary_file_path("FAIL [0000] worker-42 INFO module=core::auth"),
            None
        );
        assert!(!is_file_path_entity("Session::start"));
        assert!(is_file_path_entity("src/auth/session.rs"));
        assert_eq!(
            primary_file_path("     1 | fn handle_21() {}"),
            None,
            "live fs.read numbered lines are not a path header"
        );
    }
}
