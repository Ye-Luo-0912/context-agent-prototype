/// Cap for the hot-entity set, matching `extract_entities`'s per-text cap.
pub(crate) const MAX_HOT_ENTITIES: usize = 24;

/// Entity extraction shared across the crate (hot-set maintenance, dependency
/// linking, supersession, scoring). "Entity" is a cheap, explicit signature: a
/// whitespace-separated token of length >= 3 that carries a path/name/case
/// marker (`.`, `/`, `::`, `_` or an uppercase letter). Sorted,
/// deduplicated, bounded to 24 per text.
pub(crate) fn extract_entities(text: &str) -> Vec<String> {
    let mut entities: Vec<String> = text
        .split_whitespace()
        .map(|s| s.trim_matches(|c: char| ",;()[]{}<>\"'`".contains(c)))
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
pub(crate) fn entities_match(left: &[String], right: &[String]) -> bool {
    left.iter().any(|entity| {
        right
            .iter()
            .any(|prior| prior.contains(entity) || entity.contains(prior))
    })
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
}
