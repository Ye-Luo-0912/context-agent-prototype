use std::path::Path;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Effect, RunId, ToolExecutionFacts, ToolOutcome,
    ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec,
};
use agent_workspace::{DirectoryCreationPreparation, MAX_MUTATION_BYTES, Workspace};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::fs;

use super::{
    LineEnding, Tool, content_digest, hidden_path_output, is_hidden_name, is_not_found_error,
    missing_parent_output, missing_path_output, model_json_string, ordinary_view_blocked,
};

// A revision returned by `fs.read` must be usable by the canonical edit
// tools for every file the workspace mutation layer admits.
const MAX_READ_BYTES: u64 = MAX_MUTATION_BYTES as u64;
const MAX_WRITE_BYTES: usize = MAX_MUTATION_BYTES;
const MAX_READ_LINES: usize = 400;
const MAX_LIST_ENTRIES: usize = 2_000;

pub struct FsListTool {
    workspace: Workspace,
}

impl FsListTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: String,
    #[serde(default = "default_list_limit")]
    limit: usize,
    /// Opaque paging token returned by a previous `fs.list` call. When
    /// present, the next page is served from that call's snapshot artifact
    /// instead of a fresh directory scan, so paging stays consistent even
    /// if the directory changes between pages.
    #[serde(default)]
    /// Parser-only compatibility for non-model callers. Model-visible
    /// continuation is centralized on `artifact.read` so an opaque token is
    /// never guessed merely because every first-page call advertises it.
    cursor: Option<String>,
}

fn default_list_limit() -> usize {
    200
}

#[async_trait]
impl Tool for FsListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.list".into(),
            description: "List workspace files (hides .focus-agent and .git). Overflow returns an artifact_ref; read further lines with artifact.read.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::Search],
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ListArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.list args: {e}")))?;
        let limit = args.limit.clamp(1, MAX_LIST_ENTRIES);
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.list", &args.path,
            )));
        }
        if let Some(cursor) = args.cursor.as_deref() {
            return self
                .page_from_snapshot(run_id, call_id, cursor, limit)
                .await;
        }
        let path = match self.workspace.resolve_relative(&args.path).await {
            Ok(path) => path,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "fs.list", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let mut reader = match fs::read_dir(&path).await {
            Ok(reader) => reader,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.kind() == std::io::ErrorKind::NotADirectory =>
            {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "fs.list", &args.path).await,
                ));
            }
            Err(e) => {
                return Err(AgentError::Io(format!("list {}: {e}", path.display())));
            }
        };

        let mut entries = Vec::new();
        while let Some(entry) = reader
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(format!("read directory: {e}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_hidden_name(&name) {
                continue;
            }
            if entries.len() >= MAX_LIST_ENTRIES {
                break;
            }
            let metadata = entry.metadata().await.ok();
            let kind = metadata
                .as_ref()
                .map(|m| {
                    if m.is_dir() {
                        "dir"
                    } else if m.is_file() {
                        "file"
                    } else {
                        "other"
                    }
                })
                .unwrap_or("unknown");
            entries.push(format!("{kind}\t{name}"));
        }
        entries.sort();

        let visible = entries.iter().take(limit).cloned().collect::<Vec<_>>();
        let full = entries.join("\n");
        let artifact_ref = if entries.len() > limit {
            Some(
                self.workspace
                    .write_artifact(run_id, "fs-list", "txt", full.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        let (cursor, has_more) = match &artifact_ref {
            Some(reference) => (Some(format!("{reference}#{limit}")), true),
            None => (None, false),
        };
        let truncated_note = artifact_ref
            .as_ref()
            .map(|r| {
                format!(
                    "\n... {} more entries; full listing artifact: {r}. Continue with artifact.read reference={r} start_line={}",
                    entries.len() - visible.len(),
                    visible.len() + 1,
                )
            })
            .unwrap_or_default();

        // 目录身份戳：path@digest 让重复列举能被证据前沿识别为同版本
        // 冗余，而不是无身份的纯 stdout。根目录的相对路径是空串，用
        // "." 表示。
        let listed_relative = display_relative(&self.workspace, &path);
        let listed = if listed_relative.is_empty() {
            ".".to_string()
        } else {
            listed_relative
        };
        let list_revision = content_digest(full.as_bytes());
        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.list".into(),
            ok: true,
            summary: format!(
                "listed {} entries in {}",
                entries.len(),
                display_relative(&self.workspace, &path)
            ),
            model_content: format!("{}{}", visible.join("\n"), truncated_note),
            artifact_ref,
            metadata: json!({
                // digest 对完整 listing 计算：visible 只是分页窗口，
                // 窗口外的条目变化同样改变目录身份。
                "path": listed,
                "revision": list_revision,
                "entry_count": entries.len(),
                "returned": visible.len(),
                "has_more": has_more,
                "next_start_line": has_more.then_some(visible.len() + 1),
                "cursor": cursor,
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(&listed, Some(list_revision.clone()))])
                .with_verification(false)
                .with_mutation_bound(false),
        );
        Ok(ToolOutcome::Value(output))
    }
}

impl FsListTool {
    /// Serve one page from a previous call's snapshot artifact (cursor is
    /// `<artifact_ref>#<offset>`). Pages come from the immutable snapshot,
    /// so later changes to the directory cannot cause duplicates or gaps
    /// between pages.
    async fn page_from_snapshot(
        &self,
        run_id: RunId,
        call_id: &str,
        cursor: &str,
        limit: usize,
    ) -> AgentResult<ToolOutcome> {
        use super::{parse_cursor, read_snapshot_lines};

        let (reference, offset) = parse_cursor(cursor)?;
        let lines = read_snapshot_lines(&self.workspace, run_id, reference).await?;
        if offset > lines.len() {
            return Err(AgentError::InvalidRequest(format!(
                "cursor is past the end of the snapshot ({offset} > {} lines)",
                lines.len()
            )));
        }
        let page: Vec<&str> = lines
            .iter()
            .skip(offset)
            .take(limit)
            .map(String::as_str)
            .collect();
        let next_offset = offset + page.len();
        let has_more = next_offset < lines.len();
        let next_cursor = has_more.then(|| format!("{reference}#{next_offset}"));

        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.list".into(),
            ok: true,
            summary: format!(
                "listed entries {}-{} of {} (snapshot)",
                offset + 1,
                next_offset,
                lines.len()
            ),
            model_content: if page.is_empty() {
                "no more entries".to_string()
            } else {
                page.join("\n")
            },
            artifact_ref: Some(reference.to_string()),
            metadata: json!({
                "entry_count": lines.len(),
                "returned": page.len(),
                "has_more": has_more,
                "cursor": next_cursor,
            }),
        };
        // Snapshot pages describe no fresh directory identity; the stamp
        // keeps the explicit read-only bound so the native channel and the
        // legacy derivation agree on every `fs.list` outcome.
        output.set_native_execution_facts(
            ToolExecutionFacts::empty()
                .with_verification(false)
                .with_mutation_bound(false),
        );
        Ok(ToolOutcome::Value(output))
    }
}

pub struct FsReadTool {
    workspace: Workspace,
}

impl FsReadTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default = "default_start_line")]
    start_line: usize,
    #[serde(default = "default_end_line")]
    end_line: usize,
}

fn default_start_line() -> usize {
    1
}
fn default_end_line() -> usize {
    200
}

/// Compact physical newline map for the same logical lines `str::lines`
/// renders. It is only shown for mixed-EOL files: `C` = CRLF, `L` = LF,
/// `N` = no terminating newline. At most the configured 400-line window is
/// returned, so exposing exact edit evidence cannot grow with file size.
fn mixed_eol_tokens(text: &str, requested_start: usize, requested_end: usize) -> String {
    let bytes = text.as_bytes();
    let mut tokens = String::new();
    let mut line = 0usize;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let Some(relative_newline) = bytes[cursor..].iter().position(|byte| *byte == b'\n') else {
            if (requested_start..requested_end).contains(&line) {
                tokens.push('N');
            }
            break;
        };
        let newline = cursor + relative_newline;
        if (requested_start..requested_end).contains(&line) {
            tokens.push(if newline > cursor && bytes[newline - 1] == b'\r' {
                'C'
            } else {
                'L'
            });
        }
        line += 1;
        if line >= requested_end {
            break;
        }
        cursor = newline + 1;
    }
    tokens
}

#[async_trait]
impl Tool for FsReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.read".into(),
            description: "Read UTF-8 workspace file lines (not .focus-agent or .git), with an exact byte revision and line-ending style for safe follow-up edits.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ReadArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.read args: {e}")))?;
        if args.start_line == 0 || args.end_line < args.start_line {
            return Err(AgentError::InvalidRequest("invalid line range".into()));
        }
        // Compare the inclusive span as a difference so an adversarial
        // `usize::MAX` end cannot overflow before the boundedness check.
        if args.end_line - args.start_line >= MAX_READ_LINES {
            return Err(AgentError::InvalidRequest(format!(
                "fs.read is limited to {MAX_READ_LINES} lines per call"
            )));
        }
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.read", &args.path,
            )));
        }

        // Validation and open are fused into a directory-handle-relative
        // descent; the size check and the content read both go through the
        // pinned handle, so a link swap cannot redirect the read.
        let confined = match self.workspace.confined_open_read(&args.path).await {
            Ok(confined) => confined,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "fs.read", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let metadata = confined.metadata().map_err(|e| {
            AgentError::Io(format!("metadata {}: {e}", confined.display().display()))
        })?;
        if metadata.len() > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file is {} bytes; use a narrower/specialized tool for files above {} bytes",
                metadata.len(),
                MAX_READ_BYTES
            )));
        }

        use tokio::io::AsyncReadExt;
        let display_path = confined.display().to_path_buf();
        let file = confined.into_tokio();
        let mut text = String::new();
        file.take(MAX_READ_BYTES + 1)
            .read_to_string(&mut text)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", display_path.display())))?;
        if text.len() as u64 > MAX_READ_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file grew beyond the fs.read limit of {MAX_READ_BYTES} bytes while it was read"
            )));
        }
        let line_ending = LineEnding::detect(&text);
        let requested_start = args.start_line.saturating_sub(1);
        let requested_end = args.end_line;
        let mut line_count = 0usize;
        let mut selected = String::new();
        use std::fmt::Write as _;
        for (index, line) in text.lines().enumerate() {
            line_count = index + 1;
            if index < requested_start || index >= requested_end {
                continue;
            }
            if !selected.is_empty() {
                selected.push('\n');
            }
            write!(&mut selected, "{:>6} | {}", index + 1, line)
                .expect("writing to a String cannot fail");
        }
        let start = requested_start.min(line_count);
        let end = requested_end.min(line_count);

        let relative = display_relative(&self.workspace, &display_path);
        let quoted_relative = model_json_string(&relative);
        let revision = content_digest(text.as_bytes());
        // Metadata drives trusted Runtime freshness, but model protocol tool
        // messages carry only `model_content`. Put the edit-critical facts
        // in a compact header so the model need not infer a digest from
        // TaskProgress or load a shell to inspect mixed physical newlines.
        let mut model_content = format!(
            "file={quoted_relative} revision={revision} line_ending={}",
            line_ending.as_str()
        );
        if line_ending == LineEnding::Mixed {
            let tokens = mixed_eol_tokens(&text, requested_start, requested_end);
            model_content.push_str(" eol_tokens(C=CRLF,L=LF,N=none)=");
            model_content.push_str(&tokens);
        }
        if !selected.is_empty() {
            model_content.push('\n');
            model_content.push_str(&selected);
        }

        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: format!("read lines {}-{} of {}", start + 1, end, relative),
            model_content,
            artifact_ref: None,
            metadata: json!({
                "path": relative,
                "line_count": line_count,
                "bytes": text.len(),
                "line_ending": line_ending.as_str(),
                // The content revision (SHA-256 hex): stable for the same
                // bytes, changes with any edit — the patch tool's
                // `base_revision` precondition is checked against this.
                "revision": revision,
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(&relative, Some(revision.clone()))])
                .with_verification(false)
                .with_mutation_bound(false),
        );
        Ok(ToolOutcome::Value(output))
    }
}

pub struct FsWriteTool {
    workspace: Workspace,
}

impl FsWriteTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }

    async fn execute_inner(
        &self,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
    ) -> AgentResult<ToolOutcome> {
        let args: WriteArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("fs.write args: {e}")))?;
        if args.content.len() > MAX_WRITE_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "fs.write content is {} bytes; the limit is {MAX_WRITE_BYTES} bytes",
                args.content.len()
            )));
        }
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.write", &args.path,
            )));
        }
        let path = self.workspace.resolve_mutation(&args.path).await?;
        let transaction = match self
            .workspace
            .begin_mutation("fs.write", "write", &args.path)
            .await
        {
            Ok(transaction) => transaction,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_parent_output(&self.workspace, call_id, "fs.write", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        // Computation is staged, the side effect is not applied yet: the
        // runtime owns the commit after the generation fence. Production
        // dispatches attach Core's stable identity; direct legacy tests can
        // still exercise the transaction primitive without one.
        let prepared = match effect_context {
            Some(context) => {
                transaction
                    .prepare_with_effect_context(args.content.as_bytes(), context)
                    .await?
            }
            None => transaction.prepare(args.content.as_bytes()).await?,
        };
        let effect: Box<dyn Effect> = Box::new(prepared);
        let relative = display_relative(&self.workspace, &path);
        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: format!("wrote {} bytes to {}", args.content.len(), relative),
            model_content: format!("file updated: {relative}"),
            artifact_ref: None,
            metadata: json!({
                "path": relative,
                "bytes": args.content.len(),
                "revision": content_digest(args.content.as_bytes()),
                "line_ending": LineEnding::detect(&args.content).as_str(),
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(
                relative.as_str(),
                Some(content_digest(args.content.as_bytes())),
            )])
            .with_verification(false)
            .with_mutation_bound(true),
        );
        Ok(ToolOutcome::PreparedEffect { output, effect })
    }
}

/// Transactional creation of one directory component. It deliberately does
/// not implement `mkdir -p`: every visible topology change has one Core
/// intent, one pinned parent and one recovery identity.
pub struct FsMkdirTool {
    workspace: Workspace,
}

impl FsMkdirTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
}

#[async_trait]
impl Tool for FsMkdirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.mkdir".into(),
            description: "Create exactly one workspace directory. Its immediate parent must already exist; an existing directory succeeds without mutation.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "additionalProperties": false,
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative directory path"}
                }
            }),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: MkdirArgs = serde_json::from_value(arguments)
            .map_err(|error| AgentError::InvalidRequest(format!("fs.mkdir args: {error}")))?;
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id, "fs.mkdir", &args.path,
            )));
        }
        let preparation = match self
            .workspace
            .prepare_directory_creation("fs.mkdir", &args.path, effect_context)
            .await
        {
            Ok(preparation) => preparation,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_parent_output(&self.workspace, call_id, "fs.mkdir", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let relative = preparation.relative_path().to_string();
        match preparation {
            DirectoryCreationPreparation::AlreadyExists { .. } => {
                let mut output = ToolOutput {
                    call_id: call_id.into(),
                    tool_name: "fs.mkdir".into(),
                    ok: true,
                    summary: format!("directory already exists: {relative}"),
                    model_content: format!("directory already exists: {relative}"),
                    artifact_ref: None,
                    metadata: json!({
                        "path": relative,
                        "created": false,
                        "entry_kind": "directory",
                        "mutates_workspace": false,
                        "verification": false,
                    }),
                };
                output.set_native_execution_facts(
                    ToolExecutionFacts::from_resource_touches([(
                        relative.as_str(),
                        Option::<String>::None,
                    )])
                    .with_verification(false)
                    .with_mutation_bound(false),
                );
                Ok(ToolOutcome::Value(output))
            }
            DirectoryCreationPreparation::Prepared(prepared) => {
                let mut output = ToolOutput {
                    call_id: call_id.into(),
                    tool_name: "fs.mkdir".into(),
                    ok: true,
                    summary: format!("created directory {relative}"),
                    model_content: format!("directory created: {relative}"),
                    artifact_ref: None,
                    metadata: json!({
                        "path": relative,
                        "created": true,
                        "entry_kind": "directory",
                        "mutates_workspace": true,
                        "verification": false,
                    }),
                };
                output.set_native_execution_facts(
                    ToolExecutionFacts::from_resource_touches([(
                        relative.as_str(),
                        Option::<String>::None,
                    )])
                    .with_verification(false)
                    .with_mutation_bound(true),
                );
                Ok(ToolOutcome::PreparedEffect {
                    output,
                    effect: prepared,
                })
            }
        }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for FsWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fs.write".into(),
            description: "Write/replace a UTF-8 text file inside an existing workspace directory (maximum 4 MiB; parent directories are never created implicitly). Prefer edit.patch for existing files.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }
            }),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        self.execute_inner(call_id, arguments, effect_context).await
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
    use agent_contracts::{CancellationToken, ToolExecutionRequest, ToolFailureClass};
    use serde_json::json;

    #[test]
    fn mixed_eol_map_matches_rendered_line_windows_at_boundaries() {
        assert_eq!(mixed_eol_tokens("", 0, 400), "");
        assert_eq!(mixed_eol_tokens("a\r\nb\nc", 0, 400), "CLN");
        assert_eq!(mixed_eol_tokens("a\r\nb\nc", 1, 2), "L");
        assert_eq!(mixed_eol_tokens("a\r\nb\n", 0, 400), "CL");
        assert_eq!(mixed_eol_tokens("\n\r\nx", 0, 400), "LCN");
        assert_eq!(mixed_eol_tokens("a\rb", 0, 400), "N");
    }

    /// Every fs outcome that stamps native facts must agree with what the
    /// legacy key derivation would produce — the two channels are locked
    /// together until the legacy stamps are retired.
    #[tokio::test]
    async fn native_facts_match_the_legacy_derivation_on_every_fs_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let run_id = RunId::new();

        let write = FsWriteTool::new(workspace.clone());
        let outcome = write
            .execute(
                run_id,
                "w",
                json!({"path": "notes.txt", "content": "first"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));

        let read = FsReadTool::new(workspace.clone());
        let outcome = read
            .execute(
                run_id,
                "r",
                json!({"path": "notes.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read must return a value outcome");
        };
        crate::tools::assert_native_facts_match_derivation(&output);

        let list = FsListTool::new(workspace.clone());
        let outcome = list
            .execute(
                run_id,
                "l",
                json!({"path": "."}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.list must return a value outcome");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
    }

    #[tokio::test]
    async fn fs_write_journals_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace.clone());
        let run_id = RunId::new();

        let write = |path: &str, content: &str| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: json!({"path": path, "content": content}),
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Writing a new file stages the mutation; the runtime would commit
        // it after validating the operation — the test plays that role.
        let outcome = write("notes.txt", "first").await.unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        assert!(output.ok);
        assert!(output.heats_working_set());
        assert_eq!(output.metadata["path"], "notes.txt");
        assert_eq!(
            output.metadata["revision"].as_str().unwrap().len(),
            64,
            "write stamps a content revision"
        );
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the staged effect must commit durably"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt"))
                .await
                .unwrap(),
            "first"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["tool"], "fs.write");
        assert_eq!(record["action"], "write");
        assert_eq!(record["path"], "notes.txt");
        assert_eq!(record["bytes_before"], 0);
        assert_eq!(record["bytes_after"], 5);

        // Overwriting captures the previous content as the journal backup.
        let outcome = write("notes.txt", "second").await.unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("fs.write must prepare a committed effect");
        };
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the staged effect must commit durably"
        );
        let journal = fs::read_to_string(workspace.state_dir().join("changes.jsonl"))
            .await
            .unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        let record: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(record["old_content"], "first");
    }

    #[tokio::test]
    async fn fs_write_rejects_state_dir_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace);
        let run_id = RunId::new();
        let request = ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.write".into(),
                arguments: json!({"path": ".focus-agent/traces.jsonl", "content": "x"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        };
        let result = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::Value(output) = result else {
            panic!("hidden writes must refuse without staging");
        };
        assert!(!output.ok);
        assert_eq!(output.failure_class(), Some(ToolFailureClass::HiddenPath));
    }

    #[tokio::test]
    async fn fs_read_reports_a_stable_content_revision() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello revision\n").unwrap();
        let tool = FsReadTool::new(workspace.clone());
        let run_id = RunId::new();

        let read = |path: &str| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.read".into(),
                arguments: json!({"path": path}),
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        let first = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = first else {
            panic!("fs.read returns a plain value");
        };
        let revision_a = output.metadata["revision"].as_str().unwrap().to_string();
        assert_eq!(revision_a.len(), 64, "a full SHA-256 hex revision");
        assert_eq!(
            output.metadata["path"].as_str(),
            Some("notes.txt"),
            "fs.read must stamp the workspace-relative path; ingest cannot recover it from numbered lines"
        );

        // Same bytes, same revision.
        let second = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = second else {
            panic!("fs.read returns a plain value");
        };
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            revision_a,
            "reading the same content must not change the revision"
        );

        // Any edit changes the revision.
        std::fs::write(dir.path().join("notes.txt"), "hello revision!\n").unwrap();
        let third = read("notes.txt").await.unwrap();
        let ToolOutcome::Value(output) = third else {
            panic!("fs.read returns a plain value");
        };
        assert_ne!(
            output.metadata["revision"].as_str().unwrap(),
            revision_a,
            "an edited file must report a different revision"
        );

        // The revision is exactly the digest helper the patch tool will
        // use for its `base_revision` precondition.
        let bytes = std::fs::read(dir.path().join("notes.txt")).unwrap();
        assert_eq!(
            output.metadata["revision"].as_str().unwrap(),
            content_digest(&bytes)
        );
    }

    #[tokio::test]
    async fn fs_read_reports_crlf_without_exposing_carriage_returns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("windows.txt"), b"one\r\ntwo\r\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "windows.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };
        assert_eq!(output.metadata["line_ending"], "crlf");
        assert!(!output.model_content.contains('\r'));
        assert!(
            output
                .model_content
                .starts_with("file=\"windows.txt\" revision=")
        );
        assert!(output.model_content.contains(" line_ending=crlf\n"));
        assert_eq!(
            output.metadata["revision"],
            content_digest(b"one\r\ntwo\r\n")
        );
    }

    #[tokio::test]
    async fn fs_read_exposes_a_bounded_mixed_eol_map_without_shell() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        fs::write(dir.path().join("mixed.txt"), b"one\r\ntwo\nthree\r\nfour")
            .await
            .unwrap();
        let tool = FsReadTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "mixed.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };
        assert_eq!(output.metadata["line_ending"], "mixed");
        assert!(
            output
                .model_content
                .contains("line_ending=mixed eol_tokens(C=CRLF,L=LF,N=none)=CLCN")
        );
        assert!(output.model_content.contains(&format!(
            "revision={}",
            content_digest(b"one\r\ntwo\nthree\r\nfour")
        )));
        assert!(!output.model_content.contains('\r'));
    }

    #[tokio::test]
    async fn fs_read_newline_dense_file_returns_only_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let line_count = MAX_READ_BYTES as usize;
        std::fs::write(dir.path().join("dense.txt"), vec![b'\n'; line_count]).unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let start_line = line_count - MAX_READ_LINES + 1;
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "dense.txt",
                    "start_line": start_line,
                    "end_line": line_count,
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.read returns a value");
        };

        assert_eq!(output.metadata["line_count"], line_count);
        assert_eq!(output.model_content.lines().count(), MAX_READ_LINES + 1);
        assert!(
            output
                .model_content
                .lines()
                .nth(1)
                .is_some_and(|line| line.starts_with(&format!("{start_line:>6} | "))),
            "the selected window must retain its original line numbers"
        );
        assert!(
            output
                .model_content
                .ends_with(&format!("{line_count:>6} | ")),
            "the selected window must end at the requested line"
        );
        assert!(
            output.model_content.len() < 8 * 1024,
            "a newline-dense file must produce output proportional to the requested window"
        );
    }

    #[tokio::test]
    async fn fs_read_rejects_files_above_the_workspace_mutation_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("too-large.txt"),
            vec![b'x'; MAX_MUTATION_BYTES + 1],
        )
        .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);

        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "too-large.txt"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, AgentError::InvalidRequest(message)
                if message.contains(&MAX_READ_BYTES.to_string())),
            "fs.read and workspace mutations must reject at the same byte boundary"
        );
    }

    #[tokio::test]
    async fn fs_read_rejects_an_overflowing_line_window() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("small.txt"), b"one\n")
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);

        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "small.txt",
                    "start_line": 1,
                    "end_line": usize::MAX,
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::InvalidRequest(message) if message.contains("limited"))
        );
    }

    #[tokio::test]
    async fn fs_write_rejects_content_over_the_real_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsWriteTool::new(workspace);
        let result = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "too-large.txt", "content": "x".repeat(MAX_WRITE_BYTES + 1)}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
        assert!(!dir.path().join("too-large.txt").exists());
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn fs_list_pages_a_consistent_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        // List a subdirectory so the runtime state dir (.focus-agent) does
        // not enter the count.
        std::fs::create_dir(dir.path().join("d")).unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join("d").join(format!("file-{i:02}.txt")), "x").unwrap();
        }
        let tool = FsListTool::new(workspace.clone());
        let run_id = RunId::new();

        let list = |args: Value| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "fs.list".into(),
                arguments: args,
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        // Page 1: limit 4 of 10 entries → spills a snapshot + cursor.
        let first = list(json!({"path": "d", "limit": 4})).await.unwrap();
        let ToolOutcome::Value(output) = first else {
            panic!("fs.list returns a plain value");
        };
        assert_eq!(output.metadata["entry_count"], 10);
        assert_eq!(output.metadata["returned"], 4);
        assert_eq!(output.metadata["has_more"], true);
        assert!(
            output.artifact_ref.is_some(),
            "an overflowing listing must spill a snapshot"
        );
        let cursor = output.metadata["cursor"].as_str().unwrap().to_string();
        let first_content = output.model_content.clone();

        // The directory changes between pages — paging must not notice.
        std::fs::write(dir.path().join("d").join("zz-new.txt"), "x").unwrap();
        std::fs::remove_file(dir.path().join("d").join("file-00.txt")).unwrap();

        // Page 2 from the snapshot: exactly the next 4 snapshot entries,
        // no duplicates from page 1, no gaps, no new file leaking in.
        let second = list(json!({"path": "d", "limit": 4, "cursor": cursor}))
            .await
            .unwrap();
        let ToolOutcome::Value(output) = second else {
            panic!("fs.list returns a plain value");
        };
        let second_lines: Vec<&str> = output.model_content.lines().collect();
        assert_eq!(second_lines.len(), 4);
        assert_eq!(output.metadata["returned"], 4);
        assert_eq!(output.metadata["has_more"], true);
        let first_lines: Vec<&str> = first_content.lines().collect();
        assert!(
            second_lines.iter().all(|line| !first_lines.contains(line)),
            "pages must not overlap: {first_lines:?} vs {second_lines:?}"
        );
        assert!(
            !second_lines.iter().any(|line| line.contains("zz-new")),
            "the snapshot must not see later directory changes"
        );
        let cursor2 = output.metadata["cursor"].as_str().unwrap().to_string();

        // Page 3 drains the remaining 2 entries.
        let third = list(json!({"path": "d", "limit": 4, "cursor": cursor2}))
            .await
            .unwrap();
        let ToolOutcome::Value(output) = third else {
            panic!("fs.list returns a plain value");
        };
        assert_eq!(output.metadata["returned"], 2);
        assert_eq!(output.metadata["has_more"], false);
        assert!(output.metadata["cursor"].is_null());

        // A corrupted cursor (offset beyond the snapshot) is a clean error.
        let bad = format!("{}#9999", output.artifact_ref.as_deref().unwrap());
        let result = list(json!({"path": "d", "limit": 4, "cursor": bad})).await;
        assert!(result.is_err(), "a cursor past the snapshot must error");
    }

    #[tokio::test]
    async fn root_list_hides_runtime_and_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), "ref").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsListTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": ""}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("fs.list returns a value");
        };
        assert!(output.ok);
        assert!(output.model_content.contains("README.md"));
        assert!(!output.model_content.contains(".focus-agent"));
        assert!(!output.model_content.contains(".git"));
    }

    #[tokio::test]
    async fn fs_mkdir_prepares_one_zero_byte_effect_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsMkdirTool::new(workspace.clone());
        let outcome = tool
            .execute(
                RunId::new(),
                "mkdir-1",
                json!({"path": "src"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("an absent directory must produce a prepared effect");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
        assert!(!workspace.root().join("src").exists());
        assert_eq!(
            effect.actual_workspace_writes().unwrap(),
            vec![agent_contracts::ActualWorkspaceWrite {
                path: "src".into(),
                bytes: 0,
            }]
        );
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                ..
            }
        ));
        assert!(workspace.root().join("src").is_dir());

        let outcome = tool
            .execute(
                RunId::new(),
                "mkdir-2",
                json!({"path": "src"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("an existing directory must be an idempotent value");
        };
        crate::tools::assert_native_facts_match_derivation(&output);
        assert_eq!(output.metadata["created"], false);
        assert!(!output.may_mutate_workspace());
    }

    #[tokio::test]
    async fn fs_mkdir_and_write_explain_missing_parent_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let mkdir = FsMkdirTool::new(workspace.clone());
        let ToolOutcome::Value(missing) = mkdir
            .execute(
                RunId::new(),
                "mkdir",
                json!({"path": "missing/child"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("a missing parent must be a typed refusal");
        };
        assert_eq!(
            missing.failure_class(),
            Some(ToolFailureClass::PathNotFound)
        );
        assert_eq!(missing.metadata["next_directory"], "missing");
        assert!(missing.model_content.contains("fs.mkdir"));
        assert!(!workspace.root().join("missing").exists());

        let write = FsWriteTool::new(workspace);
        let ToolOutcome::Value(missing) = write
            .execute(
                RunId::new(),
                "write",
                json!({"path": "missing/file.txt", "content": "x"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap()
        else {
            panic!("a missing write parent must be a typed refusal");
        };
        assert_eq!(
            missing.failure_class(),
            Some(ToolFailureClass::PathNotFound)
        );
        assert_eq!(missing.metadata["next_directory"], "missing");
        assert!(missing.model_content.contains("fs.mkdir"));
    }

    #[tokio::test]
    async fn read_hidden_and_missing_paths_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src.txt"), "ok\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = FsReadTool::new(workspace);
        let run_id = RunId::new();
        let hidden = tool
            .execute(
                run_id,
                "c",
                json!({"path": ".focus-agent/changes.jsonl"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = hidden else {
            panic!("hidden read is a value");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::HiddenPath));

        let missing = tool
            .execute(
                run_id,
                "c",
                json!({"path": "src/lib.rs"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = missing else {
            panic!("missing read is a value");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::PathNotFound));
        assert!(
            output.model_content.contains("src.txt") || output.model_content.contains("parent")
        );
        assert!(
            !output.model_content.contains("Cargo.toml")
                || output.model_content.contains("Do not invent")
        );
    }
}
