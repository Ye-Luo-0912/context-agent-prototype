//! `search.grep` — rg-style regex search over workspace files.
//!
//! Model-facing output is bounded (a capped number of `file:line` hits);
//! the full hit list goes to an artifact when it overflows. Ignored
//! directories (`.git`, `.focus-agent`, `target`, `node_modules`, ...) are
//! skipped by default so build artifacts never pollute the working set.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::{Tool, display_relative, walk_files};

const MAX_FILES_SCANNED: usize = 5_000;
const MAX_BYTES_PER_FILE: u64 = 2 * 1024 * 1024;
const MODEL_HITS: usize = 100;

pub struct SearchGrepTool {
    workspace: Workspace,
}

impl SearchGrepTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Opaque paging token returned by a previous `search.grep` call. When
    /// present, the next page is served from that call's snapshot artifact
    /// instead of a fresh scan, so paging stays consistent even if files
    /// change between pages.
    #[serde(default)]
    cursor: Option<String>,
}

fn default_path() -> String {
    String::new()
}

fn default_limit() -> usize {
    200
}

#[async_trait]
impl Tool for SearchGrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search.grep".into(),
            description: "Regex search across workspace files (rg-style). Returns file:line hits, bounded; ignored dirs are skipped.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression"},
                    "path": {"type": "string", "description": "Optional workspace-relative directory"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000},
                    "cursor": {"type": "string", "description": "Opaque token from a previous search.grep result; serves the next page from that call's snapshot"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: GrepArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("search.grep args: {e}")))?;
        if let Some(cursor) = args.cursor.as_deref() {
            return self.page_from_snapshot(run_id, call_id, cursor).await;
        }
        let regex = Regex::new(&args.pattern)
            .map_err(|e| AgentError::InvalidRequest(format!("invalid regex: {e}")))?;
        let root = self.workspace.resolve_relative(&args.path).await?;

        let mut files = Vec::new();
        let mut budget = MAX_FILES_SCANNED;
        walk_files(&root, &mut files, &mut budget).await?;
        files.sort();

        let limit = args.limit.clamp(1, 1_000);
        let mut hits: Vec<String> = Vec::new();
        let mut scanned_files = 0usize;

        'files: for file in files {
            let metadata = match fs::metadata(&file).await {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.len() > MAX_BYTES_PER_FILE {
                continue;
            }
            scanned_files += 1;
            let Ok(text) = fs::read_to_string(&file).await else {
                continue;
            };
            let relative = display_relative(&self.workspace, &file);
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    hits.push(format!("{relative}:{}: {}", index + 1, line.trim_end()));
                    if hits.len() >= limit {
                        break 'files;
                    }
                }
            }
        }

        let model_hits = hits.iter().take(MODEL_HITS).cloned().collect::<Vec<_>>();
        let full = hits.join("\n");
        let artifact_ref = if hits.len() > MODEL_HITS {
            Some(
                self.workspace
                    .write_artifact(run_id, "grep", "txt", full.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        let (cursor, has_more) = match &artifact_ref {
            Some(reference) => (Some(format!("{reference}#{MODEL_HITS}")), true),
            None => (None, false),
        };
        let truncated_note = artifact_ref
            .as_ref()
            .map(|r| {
                format!(
                    "\n... {} more hits; full list: {r}",
                    hits.len() - model_hits.len()
                )
            })
            .unwrap_or_default();

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "search.grep".into(),
            ok: true,
            summary: format!(
                "{} hits for /{}/ across {} files",
                hits.len(),
                args.pattern,
                scanned_files
            ),
            model_content: if model_hits.is_empty() {
                "no matches".to_string()
            } else {
                format!("{}{}", model_hits.join("\n"), truncated_note)
            },
            artifact_ref,
            metadata: json!({
                "hits": hits.len(),
                "files_scanned": scanned_files,
                "returned": model_hits.len(),
                "has_more": has_more,
                "cursor": cursor,
            }),
        }))
    }
}

impl SearchGrepTool {
    /// Serve one page from a previous call's snapshot artifact (cursor is
    /// `<artifact_ref>#<offset>`); pages are capped at `MODEL_HITS` lines
    /// like the first page. Every page comes from the same immutable
    /// snapshot, so file changes between pages cannot cause duplicates or
    /// gaps.
    async fn page_from_snapshot(
        &self,
        _run_id: RunId,
        call_id: &str,
        cursor: &str,
    ) -> AgentResult<ToolOutcome> {
        use super::{parse_cursor, read_snapshot_lines};

        let (reference, offset) = parse_cursor(cursor)?;
        let lines = read_snapshot_lines(&self.workspace, reference).await?;
        if offset > lines.len() {
            return Err(AgentError::InvalidRequest(format!(
                "cursor is past the end of the snapshot ({offset} > {} lines)",
                lines.len()
            )));
        }
        let page: Vec<&str> = lines
            .iter()
            .skip(offset)
            .take(MODEL_HITS)
            .map(String::as_str)
            .collect();
        let next_offset = offset + page.len();
        let has_more = next_offset < lines.len();
        let next_cursor = has_more.then(|| format!("{reference}#{next_offset}"));

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "search.grep".into(),
            ok: true,
            summary: format!(
                "hit page {}-{} of {} (snapshot)",
                offset + 1,
                next_offset,
                lines.len()
            ),
            model_content: if page.is_empty() {
                "no more hits".to_string()
            } else {
                page.join("\n")
            },
            artifact_ref: Some(reference.to_string()),
            metadata: json!({
                "hits": lines.len(),
                "returned": page.len(),
                "has_more": has_more,
                "cursor": next_cursor,
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;
    use std::path::Path;
    /// Unwrap a plain tool value (search.grep never stages an effect).
    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("search.grep must return a plain value"),
        }
    }

    async fn temp_workspace() -> (Workspace, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        (workspace, dir)
    }

    async fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "search.grep".into(),
                arguments: args,
            },
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn grep_finds_matches_and_skips_ignored_dirs() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/lib.rs", "fn auth() {}\nfn main() { auth(); }\n").await;
        write(&root, "src/main.rs", "auth()\n").await;
        // Ignored: build artifacts and state dir must not be searched.
        write(&root, "target/debug/lib.rs", "auth() secret\n").await;
        write(&root, ".focus-agent/traces/x.jsonl", "auth() secret\n").await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(run_id, json!({"pattern": "auth"}));
        let output = tool
            .execute(run_id, "c", request.call.arguments, request.cancel)
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok);
        let content = output.model_content;
        assert!(
            content.contains("src/lib.rs:1"),
            "hit lib.rs line 1: {content}"
        );
        assert!(content.contains("src/main.rs:1"), "hit main.rs: {content}");
        assert!(
            !content.contains("target/") && !content.contains(".focus-agent/"),
            "ignored dirs leaked into results: {content}"
        );
        assert!(!content.contains("secret"));
    }

    #[tokio::test]
    async fn grep_bounds_model_content() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!("match_{i}: something\n"));
        }
        write(&root, "big.txt", &body).await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(run_id, json!({"pattern": "match_", "limit": 300}));
        let output = tool
            .execute(run_id, "c", request.call.arguments, request.cancel)
            .await
            .unwrap();
        let output = value(output);
        assert!(
            output.artifact_ref.is_some(),
            "overflow must go to an artifact"
        );
        assert!(
            output.model_content.matches("match_").count() <= MODEL_HITS,
            "model content exceeded the hit cap"
        );
    }

    #[tokio::test]
    async fn grep_pages_a_consistent_snapshot() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..250 {
            body.push_str(&format!("match_{i:03}: something\n"));
        }
        write(&root, "big.txt", &body).await;

        let tool = SearchGrepTool::new(workspace.clone());
        let run_id = RunId::new();

        let grep = |args: Value| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "search.grep".into(),
                arguments: args,
            };
            async move {
                tool.execute(run_id, "c", call.arguments, CancellationToken::new())
                    .await
            }
        };

        // Page 1: 250 hits, 100 shown, cursor + snapshot spill.
        let first = grep(json!({"pattern": "match_", "limit": 300}))
            .await
            .unwrap();
        let first = value(first);
        assert_eq!(first.metadata["hits"], 250);
        assert_eq!(first.metadata["has_more"], true);
        let cursor = first.metadata["cursor"].as_str().unwrap().to_string();
        assert!(first.artifact_ref.is_some());

        // The source file changes between pages; paging must not notice.
        std::fs::write(root.join("big.txt"), "match_000: changed\n").unwrap();

        // Page 2 serves the next 100 snapshot hits.
        let second = grep(json!({"pattern": "match_", "limit": 300, "cursor": cursor}))
            .await
            .unwrap();
        let second = value(second);
        assert_eq!(
            second.metadata["hits"], 250,
            "total comes from the snapshot"
        );
        assert_eq!(second.metadata["returned"], 100);
        assert!(second.metadata["has_more"].as_bool().unwrap());
        let first_lines: Vec<&str> = first.model_content.lines().collect();
        let second_lines: Vec<&str> = second.model_content.lines().collect();
        assert!(
            second_lines.iter().all(|line| !first_lines.contains(line)),
            "pages must not overlap"
        );
        assert!(
            !second_lines.iter().any(|line| line.contains("changed")),
            "the snapshot must not see later file edits"
        );

        // Drain the last 50.
        let cursor2 = second.metadata["cursor"].as_str().unwrap().to_string();
        let third = grep(json!({"pattern": "match_", "limit": 300, "cursor": cursor2}))
            .await
            .unwrap();
        let third = value(third);
        assert_eq!(third.metadata["returned"], 50);
        assert_eq!(third.metadata["has_more"], false);
        assert!(third.metadata["cursor"].is_null());

        // A malformed cursor is a clean error.
        let bad = grep(json!({"pattern": "match_", "limit": 300, "cursor": "not-a-cursor"})).await;
        assert!(bad.is_err(), "malformed cursors must error");
    }
}
