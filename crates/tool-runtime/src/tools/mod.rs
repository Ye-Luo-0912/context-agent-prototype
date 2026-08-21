mod artifact;
mod code;
mod context;
mod edit;
mod fs;
mod git;
mod patch;
mod process;
mod search;
mod session;
mod shell;
mod stream;
mod task;
mod view;

pub(crate) use artifact::ArtifactReadTool;
pub(crate) use code::{CodeDiagnosticsTool, CodeSymbolsTool};
pub(crate) use context::ContextManageTool;
pub(crate) use edit::EditReplaceTool;
pub(crate) use fs::{FsListTool, FsReadTool, FsWriteTool};
pub(crate) use git::{GitDiffTool, GitStatusTool};
pub(crate) use patch::EditPatchTool;
pub(crate) use process::ProcessRunTool;
pub(crate) use search::SearchGrepTool;
pub(crate) use session::{ProcessSession, ProcessSessionTool};
pub(crate) use shell::ShellExecTool;
pub use shell::{ShellDialect, ShellKind};
pub(crate) use task::TaskCompleteTool;
pub(crate) use view::{
    hidden_path_output, is_hidden_name, is_not_found_error, missing_path_output,
    ordinary_view_blocked,
};

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, OperationEffectContext, RunId, ToolOutcome,
    ToolSpec, is_non_transactional_process_tool, process_spawn_is_covered,
};
use agent_process::kill_process_tree;
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde_json::Value;
use std::borrow::Cow;
use std::path::Path;
use tokio::fs as tokio_fs;
use tokio::io::AsyncReadExt;

/// Directories the workspace scanners skip by default, so build artifacts
/// and runtime state never pollute the working set.
pub(crate) const IGNORED_DIRS: &[&str] = &[
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

pub(crate) fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

/// 每处理这么多目录项就协作让出一次，让 `search.grep` 的取消能打断 walk。
const WALK_YIELD_EVERY: u32 = 32;

/// Depth-first walk collecting regular files under `root`, honoring
/// `IGNORED_DIRS` and stopping once `budget` files have been collected.
///
/// `cancel` 为 `Some` 时在目录项之间检查 token 并协作让出；已收集的路径留在
/// `out` 中。`code.symbols` 传 `None`，walk 语义与取消前一致。
pub(crate) async fn walk_files(
    root: &Path,
    out: &mut Vec<std::path::PathBuf>,
    budget: &mut usize,
    cancel: Option<&CancellationToken>,
) -> AgentResult<()> {
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    let mut entries_since_yield = 0u32;
    while let Some(dir) = stack.pop() {
        if *budget == 0 {
            return Ok(());
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(());
        }
        let mut reader = tokio_fs::read_dir(&dir)
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
            if let Some(token) = cancel {
                if token.is_cancelled() {
                    return Ok(());
                }
                entries_since_yield += 1;
                if entries_since_yield >= WALK_YIELD_EVERY {
                    entries_since_yield = 0;
                    tokio::task::yield_now().await;
                }
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

/// Render a path relative to the workspace with forward slashes, so tool
/// results are stable across platforms.
pub(crate) fn display_relative(workspace: &Workspace, path: &Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The file-revision identity shared by the read and patch tools: the
/// SHA-256 of the file's bytes as a lowercase hex string. `fs.read`
/// reports it (`revision`), and `edit.patch` requires it as
/// `base_revision`, so an edit is refused unless the file is exactly the
/// revision the model based its change on (TOOLS-05 file-revision
/// semantics).
pub(crate) fn content_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The line-ending shape of the current on-disk text. Exact edits preserve
/// a uniform target style, but deliberately leave mixed/legacy files
/// byte-exact instead of guessing how they should be rewritten.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LineEnding {
    None,
    Lf,
    CrLf,
    Mixed,
}

impl LineEnding {
    pub(crate) fn detect(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut lf = 0usize;
        let mut crlf = 0usize;
        let mut lone_cr = 0usize;
        let mut index = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                    crlf += 1;
                    index += 2;
                }
                b'\r' => {
                    lone_cr += 1;
                    index += 1;
                }
                b'\n' => {
                    lf += 1;
                    index += 1;
                }
                _ => index += 1,
            }
        }
        match (lf, crlf, lone_cr) {
            (0, 0, 0) => Self::None,
            (0, _, 0) => Self::CrLf,
            (_, 0, 0) => Self::Lf,
            _ => Self::Mixed,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lf => "lf",
            Self::CrLf => "crlf",
            Self::Mixed => "mixed",
        }
    }
}

/// Normalize only the representation of line breaks in model-provided edit
/// text. This is not fuzzy matching: every non-line-ending byte must still
/// match exactly, and mixed target files stay fully byte-exact. A uniform
/// target keeps its existing style so an LF-formatted tool argument can edit
/// a CRLF Windows file without producing a mixed-ending result.
pub(crate) fn normalize_edit_line_endings(text: &str, target: LineEnding) -> Cow<'_, str> {
    match target {
        LineEnding::CrLf => {
            if text.contains('\n') && LineEnding::detect(text) != LineEnding::CrLf {
                Cow::Owned(text.replace("\r\n", "\n").replace('\n', "\r\n"))
            } else {
                Cow::Borrowed(text)
            }
        }
        LineEnding::Lf => {
            if text.contains("\r\n") {
                Cow::Owned(text.replace("\r\n", "\n"))
            } else {
                Cow::Borrowed(text)
            }
        }
        LineEnding::None | LineEnding::Mixed => Cow::Borrowed(text),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExactMatch {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactMatchError {
    NoMatch { count: usize },
    Ambiguous { count: usize },
}

/// Locate an exact (non-overlapping) occurrence in one scan and constant
/// auxiliary memory. The previous implementation collected every match,
/// which could allocate millions of offsets for a small needle in a 4 MiB
/// file even though only one offset was needed.
pub(crate) fn exact_match(
    text: &str,
    needle: &str,
    occurrence: Option<usize>,
) -> Result<ExactMatch, ExactMatchError> {
    let mut count = 0usize;
    let mut selected = None;
    for (start, matched) in text.match_indices(needle) {
        count += 1;
        if occurrence == Some(count) || (occurrence.is_none() && count == 1) {
            selected = Some((start, start + matched.len()));
        }
    }
    match (occurrence, selected, count) {
        (Some(requested), Some((start, end)), _) if requested > 0 => {
            Ok(ExactMatch { start, end, count })
        }
        (Some(_), _, _) | (None, None, 0) => Err(ExactMatchError::NoMatch { count }),
        (None, Some((start, end)), 1) => Ok(ExactMatch { start, end, count }),
        (None, _, _) => Err(ExactMatchError::Ambiguous { count }),
    }
}

/// Project a replacement before allocating the result. All text mutators
/// use the same file-size ceiling for both input and output, preventing a
/// small replace-all request from expanding into an unbounded allocation.
pub(crate) fn projected_replacement_len(
    current: usize,
    old: usize,
    new: usize,
    replacements: usize,
) -> Option<usize> {
    if new >= old {
        current.checked_add(new.checked_sub(old)?.checked_mul(replacements)?)
    } else {
        current.checked_sub(old.checked_sub(new)?.checked_mul(replacements)?)
    }
}

const MAX_EDIT_CANDIDATES: usize = 3;
const CANDIDATE_CONTEXT_LINES: usize = 2;
const CANDIDATE_MAX_CHARS: usize = 400;

/// Bounded line-numbered windows around exact or first-line probes.
/// Never used to authorize a fuzzy mutation.
pub(crate) fn candidate_regions(text: &str, needle: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, _) in text.match_indices(needle).take(MAX_EDIT_CANDIDATES) {
        out.push(region_at(text, idx));
    }
    if !out.is_empty() {
        return out;
    }
    let probe = needle
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(needle);
    let probe = if probe.chars().count() > 48 {
        probe.chars().take(48).collect::<String>()
    } else {
        probe.to_string()
    };
    if probe.trim().is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        if line.contains(&probe) {
            out.push(window_at(&lines, index));
            if out.len() >= MAX_EDIT_CANDIDATES {
                break;
            }
        }
    }
    out
}

fn region_at(text: &str, byte_index: usize) -> String {
    let before = text[..byte_index].lines().count().saturating_sub(1);
    let lines: Vec<&str> = text.lines().collect();
    window_at(&lines, before)
}

fn window_at(lines: &[&str], center: usize) -> String {
    let start = center.saturating_sub(CANDIDATE_CONTEXT_LINES);
    let end = (center + CANDIDATE_CONTEXT_LINES + 1).min(lines.len());
    let mut block = String::new();
    for (offset, line) in lines[start..end].iter().enumerate() {
        let number = start + offset + 1;
        let clipped: String = line.chars().take(120).collect();
        block.push_str(&format!("{number:>6} | {clipped}\n"));
    }
    if block.chars().count() > CANDIDATE_MAX_CHARS {
        block.chars().take(CANDIDATE_MAX_CHARS).collect()
    } else {
        block
    }
}

/// Context lines shown around the changed region of an after-edit echo.
const EDIT_ECHO_CONTEXT_LINES: usize = 3;
/// Hard char cap for one after-edit echo. The echo is transient
/// per-decision information — it replaces a confirm `fs.read` round, and
/// a later same-path echo supersedes it — so the bound keeps even a
/// whole-file rewrite from turning the success line into a second file
/// dump. `fs.read` stays the full-body path.
pub(crate) const EDIT_ECHO_MAX_CHARS: usize = 1200;

/// Keep a model-facing fragment inside a hard character budget, including
/// the truncation marker itself. This is shared by single- and multi-file
/// edit receipts so adding files can never multiply an advertised cap.
pub(crate) fn bound_chars_with_marker(text: &str, max_chars: usize, marker: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let marker_len = marker.chars().count();
    if marker_len >= max_chars {
        return marker.chars().take(max_chars).collect();
    }
    let mut bounded: String = text.chars().take(max_chars - marker_len).collect();
    bounded.push_str(marker);
    bounded
}

/// Byte span `[start, end)` of the region that differs between the
/// original and the updated content, expressed in the updated text.
/// Common prefix/suffix, snapped to char boundaries.
fn changed_span(original: &str, updated: &str) -> (usize, usize) {
    let a = original.as_bytes();
    let b = updated.as_bytes();
    let mut prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()).saturating_sub(prefix);
    let mut suffix = 0usize;
    while suffix < max_suffix && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix] {
        suffix += 1;
    }
    while prefix > 0 && !updated.is_char_boundary(prefix) {
        prefix -= 1;
    }
    while suffix > 0 && !updated.is_char_boundary(updated.len() - suffix) {
        suffix -= 1;
    }
    (prefix, updated.len() - suffix)
}

/// Bounded after-edit echo: a line-numbered window of the *updated*
/// content covering everything the edit changed (plus context), rendered
/// like the refusal candidate windows. A successful edit currently tells
/// the model nothing about the new text, which forces a confirm
/// `fs.read` before the next chained edit; the echo removes that round
/// while staying transient — it is superseded by the next same-path echo
/// and compacted with the exchange, never a residency commitment.
pub(crate) fn edit_echo(original: &str, updated: &str, max_chars: usize) -> String {
    let (span_start, span_end) = changed_span(original, updated);
    if updated.lines().next().is_none() {
        return "(file is now empty)".to_string();
    }
    let first_line = updated[..span_start].matches('\n').count();
    let last_line = if span_end > span_start {
        // Probe the byte just before the span end, snapped down to a
        // char boundary (the span end itself may sit mid-character).
        let mut probe = span_end - 1;
        while probe > 0 && !updated.is_char_boundary(probe) {
            probe -= 1;
        }
        updated[..probe].matches('\n').count()
    } else {
        first_line
    };
    let window_start = first_line.saturating_sub(EDIT_ECHO_CONTEXT_LINES);
    let take = last_line + 1 + EDIT_ECHO_CONTEXT_LINES - window_start;
    let mut out = String::new();
    for (index, line) in updated.lines().enumerate().skip(window_start).take(take) {
        let number = index + 1;
        let clipped: String = line.chars().take(120).collect();
        let rendered = format!("{number:>6} | {clipped}\n");
        if out.chars().count() + rendered.chars().count() > max_chars {
            out.push_str(&rendered);
            return bound_chars_with_marker(
                &out,
                max_chars,
                "… (echo truncated at cap; fs.read the path for the full body)\n",
            );
        }
        out.push_str(&rendered);
    }
    out
}

pub(crate) fn classify_process_outcome(
    outcome: &str,
    exit_ok: bool,
    output_tail: &str,
    command: Option<&str>,
    dialect: Option<&ShellDialect>,
    markers: &[String],
) -> Option<agent_contracts::ToolFailureClass> {
    use agent_contracts::ToolFailureClass;
    if outcome == "cancelled" {
        return Some(ToolFailureClass::Cancellation);
    }
    if outcome == "timed out" {
        return Some(ToolFailureClass::Timeout);
    }
    if exit_ok {
        return None;
    }
    let tail = output_tail.to_ascii_lowercase();
    if looks_unavailable(&tail) {
        if dialect.is_some_and(|d| d.kind.wrong_dialect_likely(command.unwrap_or(""), &tail)) {
            return Some(ToolFailureClass::ShellDialectMismatch);
        }
        return Some(ToolFailureClass::CommandUnavailable);
    }
    if let Some(command) = command
        && let Some(marker) = required_project_marker(command)
        && !marker_present(markers, marker)
        && marker_missing_evidence(marker, &tail)
    {
        return Some(ToolFailureClass::MissingProjectMarker);
    }
    if dialect.is_some_and(|d| d.kind.wrong_dialect_likely(command.unwrap_or(""), &tail)) {
        return Some(ToolFailureClass::ShellDialectMismatch);
    }
    Some(ToolFailureClass::ProcessExit)
}

pub(crate) fn required_project_marker(command: &str) -> Option<&'static str> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let exe = normalize_exe(tokens.first()?)?;
    let sub = tokens
        .get(1)
        .map(|token| token.trim_matches('"').to_ascii_lowercase());
    match (exe.as_str(), sub.as_deref()) {
        ("cargo", Some("test" | "build" | "run" | "check" | "clippy" | "fmt" | "bench")) => {
            Some("Cargo.toml")
        }
        ("npm", Some("test" | "install" | "ci" | "run")) => Some("package.json"),
        ("yarn", Some("test" | "install" | "run")) => Some("package.json"),
        ("pnpm", Some("test" | "install" | "run")) => Some("package.json"),
        ("go", Some("test" | "build" | "run" | "mod")) => Some("go.mod"),
        ("mvn", Some("test" | "package" | "install")) => Some("pom.xml"),
        _ => None,
    }
}

fn normalize_exe(token: &str) -> Option<String> {
    let token = token.trim_matches('"');
    let token = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .trim_end_matches(".bat")
        .to_ascii_lowercase();
    if token.is_empty() { None } else { Some(token) }
}

fn marker_missing_evidence(marker: &str, tail: &str) -> bool {
    let needle = marker.to_ascii_lowercase();
    tail.contains(&needle)
        && (tail.contains("could not find")
            || tail.contains("no such file")
            || tail.contains("cannot find")
            || tail.contains("not found")
            || tail.contains("enoent"))
}

fn marker_present(markers: &[String], needed: &str) -> bool {
    if markers.iter().any(|marker| marker == needed) {
        return true;
    }
    needed == "pyproject.toml"
        && markers
            .iter()
            .any(|marker| marker == "requirements.txt" || marker == "setup.py")
}

fn looks_unavailable(tail: &str) -> bool {
    tail.contains("is not recognized")
        || tail.contains("commandnotfoundexception")
        || tail.contains("not found")
        || tail.contains("no such file or directory")
        || tail.contains("is not recognized as a name of a cmdlet")
}

/// Upper bound for reading a spilled snapshot back during cursor paging.
/// Snapshots are constructed under their own caps (fs.list ≤ 2000 entries,
/// search.grep ≤ 1000 hits), so this is a defensive ceiling, not a budget.
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;

/// Parse a snapshot paging cursor: `<artifact_ref>#<line_offset>`. Identity
/// locators never contain `#`, so the last `#` is the offset; a crafted
/// cursor with extra `#`s fails `ArtifactLocator::parse`.
pub(crate) fn parse_cursor(cursor: &str) -> AgentResult<(&str, usize)> {
    let (reference, offset) = cursor.rsplit_once('#').ok_or_else(|| {
        AgentError::InvalidRequest(format!(
            "malformed cursor (expected <artifact_ref>#<offset>): {cursor:?}"
        ))
    })?;
    let offset: usize = offset
        .parse()
        .map_err(|_| AgentError::InvalidRequest(format!("malformed cursor offset: {cursor:?}")))?;
    Ok((reference, offset))
}

/// Read a spilled snapshot artifact (bounded) and return its lines. Cursor
/// paging serves every page from the *same immutable snapshot*, so the
/// paging is consistent: changes to the underlying directory or file set
/// between pages cannot cause duplicates or gaps.
pub(crate) async fn read_snapshot_lines(
    workspace: &Workspace,
    run_id: RunId,
    reference: &str,
) -> AgentResult<Vec<String>> {
    let (_normalized, confined) = workspace.open_artifact_for_run(reference, run_id).await?;
    let file = confined.into_tokio();
    let mut bytes = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| AgentError::Io(format!("read snapshot artifact: {e}")))?;
    if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err(AgentError::InvalidRequest(format!(
            "snapshot artifact exceeds {MAX_SNAPSHOT_BYTES} bytes"
        )));
    }
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .map(String::from)
        .collect())
}

/// spawn 成功后立刻记下 PID；没有 Core 身份时杀树并失败，避免留下无证据孩子。
pub(crate) fn persist_spawned_process(
    workspace: &Workspace,
    effect_context: &Option<OperationEffectContext>,
    child: &tokio::process::Child,
    expected_tool_name: &str,
) -> AgentResult<u32> {
    let pid = child
        .id()
        .ok_or_else(|| AgentError::Tool("spawned process has no pid".into()))?;
    match require_process_effect_context(effect_context, expected_tool_name) {
        Ok(context) => {
            workspace.record_process_spawn(context, pid)?;
            Ok(pid)
        }
        Err(error) => {
            kill_process_tree(pid);
            Err(error)
        }
    }
}

/// Non-transactional process tools may not spawn without Core-issued identity.
/// The identity's tool name must be this builtin; a fs.write lease cannot
/// authorize a shell child.
pub(crate) fn require_process_effect_context<'a>(
    effect_context: &'a Option<OperationEffectContext>,
    expected_tool_name: &str,
) -> AgentResult<&'a OperationEffectContext> {
    let Some(context) = effect_context.as_ref() else {
        return Err(AgentError::InvalidRequest(
            "non-transactional process tools cannot spawn without Core-issued effect identity"
                .into(),
        ));
    };
    context.validate().map_err(AgentError::InvalidRequest)?;
    if !is_non_transactional_process_tool(expected_tool_name) {
        return Err(AgentError::InvalidRequest(format!(
            "'{expected_tool_name}' is not a non-transactional process tool"
        )));
    }
    if context.identity.tool_name != expected_tool_name {
        return Err(AgentError::InvalidRequest(format!(
            "process spawn identity is for '{}' but this tool is '{expected_tool_name}'",
            context.identity.tool_name
        )));
    }
    Ok(context)
}

pub(crate) fn require_covered_process_spawn(
    tool_name: &str,
    arguments: &Value,
    actual: &agent_contracts::EffectIntent,
) -> AgentResult<()> {
    if process_spawn_is_covered(tool_name, arguments, actual) {
        Ok(())
    } else {
        Err(AgentError::InvalidRequest(
            "actual process command is not covered by the approved effect intent; the child was not started".into(),
        ))
    }
}

pub(crate) fn persist_process_exit(
    workspace: &Workspace,
    pid: u32,
    exit_code: Option<i32>,
) -> AgentResult<()> {
    workspace.record_process_exit(pid, exit_code)
}

pub(crate) fn abandon_spawned_process(child: &mut tokio::process::Child) {
    kill_process_tree(child.id().unwrap_or(0));
}

#[cfg(test)]
pub(crate) fn test_process_effect_context(
    run_id: RunId,
    call_id: &str,
    tool_name: &str,
    arguments: &Value,
) -> OperationEffectContext {
    use agent_contracts::{ArgumentDigest, EffectId, OperationId, ToolOperationIdentity, TurnId};
    OperationEffectContext {
        identity: ToolOperationIdentity {
            run_id,
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id: OperationId::new(),
            generation: 1,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            argument_digest: ArgumentDigest::from_json(arguments),
        },
        effect_id: EffectId::new(),
    }
}

#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome>;
}

#[cfg(test)]
mod classify_tests {
    use super::*;
    use agent_contracts::ToolFailureClass;

    #[test]
    fn rustc_and_generic_tools_are_not_missing_project_marker() {
        assert_eq!(
            required_project_marker("rustc --test src/protocol.rs"),
            None
        );
        assert_eq!(required_project_marker("pytest"), None);
        assert_eq!(required_project_marker("pip install foo"), None);
        assert_eq!(required_project_marker("npx tsc"), None);
        assert_eq!(
            classify_process_outcome(
                "exited",
                false,
                "error: couldn't compile src/protocol.rs",
                Some("rustc --test src/protocol.rs"),
                None,
                &[],
            ),
            Some(ToolFailureClass::ProcessExit)
        );
    }

    #[test]
    fn cargo_test_missing_marker_requires_subcommand_evidence_and_absence() {
        assert_eq!(required_project_marker("cargo test"), Some("Cargo.toml"));
        assert_eq!(
            classify_process_outcome(
                "exited",
                false,
                "error: could not find `Cargo.toml` in `/tmp/x` or any parent directory",
                Some("cargo test"),
                None,
                &[],
            ),
            Some(ToolFailureClass::MissingProjectMarker)
        );
        assert_eq!(
            classify_process_outcome(
                "exited",
                false,
                "error: test failed",
                Some("cargo test"),
                None,
                &[],
            ),
            Some(ToolFailureClass::ProcessExit)
        );
        assert_eq!(
            classify_process_outcome(
                "exited",
                false,
                "error: could not find `Cargo.toml`",
                Some("cargo test"),
                None,
                &["Cargo.toml".into()],
            ),
            Some(ToolFailureClass::ProcessExit)
        );
    }

    #[test]
    fn unavailable_binary_is_not_missing_project_marker() {
        assert_eq!(
            classify_process_outcome(
                "exited",
                false,
                "'cargo' is not recognized as an internal or external command",
                Some("cargo test"),
                None,
                &[],
            ),
            Some(ToolFailureClass::CommandUnavailable)
        );
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    fn sleeper() -> tokio::process::Command {
        #[cfg(windows)]
        {
            let mut command = tokio::process::Command::new("ping");
            command.args(["-n", "20", "127.0.0.1"]);
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = tokio::process::Command::new("sleep");
            command.arg("20");
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());
            command
        }
    }

    #[tokio::test]
    async fn persist_without_identity_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let mut child = sleeper().spawn().unwrap();
        let error = persist_spawned_process(&workspace, &None, &child, "shell.exec").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot spawn without Core-issued effect identity"),
            "{error}"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            child.try_wait().unwrap().is_some(),
            "an unmanaged child must not be left running"
        );
    }

    #[tokio::test]
    async fn persist_rejects_a_mismatched_tool_identity() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let mut child = sleeper().spawn().unwrap();
        let arguments = serde_json::json!({"argv": ["sleep", "1"]});
        let context = Some(test_process_effect_context(
            RunId::new(),
            "c",
            "process.run",
            &arguments,
        ));
        let error =
            persist_spawned_process(&workspace, &context, &child, "shell.exec").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("process spawn identity is for 'process.run'"),
            "{error}"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            child.try_wait().unwrap().is_some(),
            "a mismatched-identity child must not be left running"
        );
    }
}

#[cfg(test)]
mod echo_tests {
    use super::*;

    #[test]
    fn echo_covers_the_changed_region_with_context_and_numbers() {
        let original = "l1\nl2\nl3\nl4\nfn b() {}\nl6\nl7\nl8\nl9\nl10\n";
        let updated = original.replace("fn b() {}", "fn b() -> u8 { 1 }");
        let echo = edit_echo(original, &updated, EDIT_ECHO_MAX_CHARS);
        // The changed line (5) and its ±3 context appear, line-numbered
        // like the fs.read / refusal renderers.
        assert!(echo.contains("     2 | l2"), "{echo}");
        assert!(echo.contains("     5 | fn b() -> u8 { 1 }"), "{echo}");
        assert!(echo.contains("     8 | l8"), "{echo}");
        // Context is bounded: lines 1 and 9 stay out of the one-line edit.
        assert!(!echo.contains("     1 | l1"), "{echo}");
        assert!(!echo.contains("     9 | l9"), "{echo}");
    }

    #[test]
    fn echo_marks_truncation_and_points_to_fs_read() {
        // One long line stays under a 200-char budget after the 120-char
        // per-line clip, so truncation needs many changed lines to bite.
        let original = "one\n";
        let updated = original.replace(
            "one",
            &(0..60)
                .map(|index| format!("line {index} padding padding padding\n"))
                .collect::<String>(),
        );
        let echo = edit_echo(original, &updated, 200);
        assert!(
            echo.contains("fs.read the path for the full body"),
            "a capped echo must point at the full-body path: {echo}"
        );
        assert!(
            echo.chars().count() <= 200,
            "the hard cap must hold: {} chars",
            echo.chars().count()
        );
    }

    #[test]
    fn echo_handles_deletion_insertion_and_empty_results() {
        // Deletion: the window centers on the point where text vanished.
        let echo = edit_echo("a\nXX\nb\n", "a\nb\n", EDIT_ECHO_MAX_CHARS);
        assert!(echo.contains("     2 | b"), "{echo}");

        // Insertion into an empty file shows the new line.
        let echo = edit_echo("", "hello\n", EDIT_ECHO_MAX_CHARS);
        assert!(echo.contains("     1 | hello"), "{echo}");

        // Everything deleted: no lines to show, say so.
        let echo = edit_echo("gone\n", "", EDIT_ECHO_MAX_CHARS);
        assert_eq!(echo, "(file is now empty)");
    }

    #[test]
    fn echo_spans_multi_line_changes_and_multibyte_boundaries() {
        // A multi-line replacement is covered end to end.
        let original = "header\nbody-1\nbody-2\nbody-3\nfooter\n";
        let updated = original.replace("body-1\nbody-2\nbody-3", "body-A\nbody-B");
        let echo = edit_echo(original, &updated, EDIT_ECHO_MAX_CHARS);
        assert!(echo.contains("body-A"), "{echo}");
        assert!(echo.contains("body-B"), "{echo}");
        assert!(echo.contains("footer"), "{echo}");

        // Multibyte content never panics on char boundaries.
        let original = "ααα\nβββ\n";
        let updated = original.replace("βββ", "βγδ");
        let echo = edit_echo(original, &updated, EDIT_ECHO_MAX_CHARS);
        assert!(echo.contains("βγδ"), "{echo}");
    }

    #[test]
    fn line_ending_normalization_is_exact_and_preserves_uniform_targets() {
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\nb\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\r\nb\n"), LineEnding::Mixed);
        assert_eq!(LineEnding::detect("plain"), LineEnding::None);

        assert_eq!(
            normalize_edit_line_endings("a\nb\n", LineEnding::CrLf),
            "a\r\nb\r\n"
        );
        assert_eq!(
            normalize_edit_line_endings("a\r\nb\r\n", LineEnding::Lf),
            "a\nb\n"
        );
        // Mixed targets are never guessed at or silently rewritten.
        assert!(matches!(
            normalize_edit_line_endings("a\nb", LineEnding::Mixed),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn exact_match_uses_constant_state_and_keeps_ambiguity_explicit() {
        assert_eq!(
            exact_match("a b a", "a", Some(2)).unwrap(),
            ExactMatch {
                start: 4,
                end: 5,
                count: 2
            }
        );
        assert_eq!(
            exact_match("a b a", "a", None),
            Err(ExactMatchError::Ambiguous { count: 2 })
        );
        assert_eq!(
            exact_match("abc", "z", None),
            Err(ExactMatchError::NoMatch { count: 0 })
        );
    }

    #[test]
    fn projected_replacement_size_is_checked_before_allocation() {
        assert_eq!(projected_replacement_len(10, 1, 3, 2), Some(14));
        assert_eq!(projected_replacement_len(10, 3, 1, 2), Some(6));
        assert_eq!(
            projected_replacement_len(usize::MAX, 1, usize::MAX, 2),
            None
        );
    }
}
