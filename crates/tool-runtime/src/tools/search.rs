//! `search.grep` — rg-style regex search over workspace files.
//!
//! Model-facing output is bounded (a capped number of `file:line` hits);
//! the full hit list goes to an artifact when it overflows. Ignored
//! directories (`.git`, `.focus-agent`, `target`, `node_modules`, ...) are
//! skipped by default so build artifacts never pollute the working set.

use std::path::Path;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::Tool;

const MAX_FILES_SCANNED: usize = 5_000;
const MAX_BYTES_PER_FILE: u64 = 2 * 1024 * 1024;
const MODEL_HITS: usize = 100;

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".focus-agent",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    ".idea",
    ".vscode",
];

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
}

fn default_path() -> String {
    String::new()
}

fn default_limit() -> usize {
    200
}

fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

async fn walk_files(
    root: &Path,
    out: &mut Vec<std::path::PathBuf>,
    budget: &mut usize,
) -> AgentResult<()> {
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if *budget == 0 {
            return Ok(());
        }
        let mut reader = fs::read_dir(&dir)
            .await
            .map_err(|e| AgentError::Io(format!("read dir {}: {e}", dir.display())))?;
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(format!("read dir entry: {e}")))?
        {
            if *budget == 0 {
                return Ok(());
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .await
                .map_err(|e| AgentError::Io(format!("file type: {e}")))?;
            if file_type.is_dir() {
                if is_ignored_dir(&name) {
                    continue;
                }
                stack.push(entry.path());
            } else if file_type.is_file() {
                *budget = budget.saturating_sub(1);
                out.push(entry.path());
            }
        }
    }
    Ok(())
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
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                }
            }),
            risk: ToolRisk::ReadOnly,
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
            metadata: json!({"hits": hits.len(), "files_scanned": scanned_files}),
        }))
    }
}

fn display_relative(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;

    /// Unwrap a plain tool value (search.grep never stages an effect).
    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. } | ToolOutcome::RuntimeDirective { .. } => {
                panic!("search.grep must return a plain value")
            }
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
}
