//! Read-only git tools (`git.status`, `git.diff`).
//!
//! Both run git inside the workspace, bound the model-facing output to a tail,
//! and store a bounded output prefix as an artifact when it overflows.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSemanticRole, ToolSpec,
};
use agent_process::kill_process_tree;
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Duration, Instant, timeout, timeout_at};

use super::Tool;

const GIT_TIMEOUT_MS: u64 = 20_000;
const MODEL_OUTPUT_CHARS: usize = 12_000;
const MAX_PIPE_BYTES: usize = super::stream::MAX_ARTIFACT_BYTES / 2;
const MAX_DISCOVERY_BYTES: usize = 64 * 1024;

struct CapturedPipe {
    bytes: Vec<u8>,
    total: usize,
}

async fn drain_pipe(mut pipe: impl AsyncRead + Unpin, cap: usize) -> std::io::Result<CapturedPipe> {
    let mut captured = CapturedPipe {
        bytes: Vec::new(),
        total: 0,
    };
    let mut buffer = [0; 8192];
    loop {
        let read = pipe.read(&mut buffer).await?;
        if read == 0 {
            return Ok(captured);
        }
        captured.total = captured.total.saturating_add(read);
        let keep = read.min(cap.saturating_sub(captured.bytes.len()));
        captured.bytes.extend_from_slice(&buffer[..keep]);
    }
}

/// Do not inherit Git routing, injected config, trace destinations or drivers.
/// Repository discovery below uses only non-executing plumbing commands.
fn git_command(workspace: &Workspace) -> AgentResult<Command> {
    // Resolve only absolute host PATH entries. In particular, Windows must
    // not prefer a workspace git.exe through its implicit current-dir search.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let program = std::env::split_paths(&path)
        .filter(|path| path.is_absolute())
        .map(|path| path.join(if cfg!(windows) { "git.exe" } else { "git" }))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            AgentError::Tool("git executable not found on the absolute host PATH".into())
        })?;
    let mut command = Command::new(program);
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
        ])
        .current_dir(workspace.root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    Ok(command)
}

async fn capture_git(
    mut command: Command,
    cancel: &CancellationToken,
    deadline: Instant,
    cap: usize,
) -> AgentResult<(std::process::ExitStatus, CapturedPipe, CapturedPipe)> {
    if cancel.is_cancelled() {
        return Err(AgentError::Cancelled);
    }
    let mut child = command
        .spawn()
        .map_err(|e| AgentError::Tool(format!("run git: {e}")))?;
    let pid = child.id().unwrap_or(0);
    let mut guard = super::ProcessTreeGuard::new(pid);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // All three futures are polled together. The deadline includes pipe EOF,
    // even when the direct child has already exited.
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AgentError::Cancelled),
        result = timeout_at(deadline, async {
            tokio::try_join!(async {
                let status = child.wait().await?;
                guard.disarm();
                Ok(status)
            }, drain_pipe(stdout, cap), drain_pipe(stderr, cap))
        }) => match result {
            Ok(result) => result.map_err(|e| AgentError::Tool(format!("read/wait git: {e}"))),
            Err(_) => Err(AgentError::Tool("git timed out".into())),
        },
    };
    if result.is_err() {
        if !matches!(child.try_wait(), Ok(Some(_))) {
            kill_process_tree(pid);
            let _ = child.start_kill();
            if matches!(
                timeout(Duration::from_secs(5), child.wait()).await,
                Ok(Ok(_))
            ) {
                guard.disarm();
            }
        } else {
            guard.disarm();
        }
    }
    result
}

async fn discover(
    workspace: &Workspace,
    args: &[&str],
    cancel: &CancellationToken,
    deadline: Instant,
    allow_missing: bool,
) -> AgentResult<String> {
    let mut command = git_command(workspace)?;
    command.args(args);
    let (status, stdout, stderr) =
        capture_git(command, cancel, deadline, MAX_DISCOVERY_BYTES).await?;
    if stdout.total > MAX_DISCOVERY_BYTES || stderr.total > MAX_DISCOVERY_BYTES {
        return Err(AgentError::Tool(
            "git repository metadata exceeds the discovery limit".into(),
        ));
    }
    if !status.success() && !(allow_missing && status.code() == Some(1)) {
        return Err(AgentError::Tool(format!(
            "git repository discovery failed: {}",
            strip_runtime_paths(&String::from_utf8_lossy(&stderr.bytes))
        )));
    }
    String::from_utf8(stdout.bytes)
        .map_err(|_| AgentError::Tool("git repository metadata is not UTF-8".into()))
}

/// A private Git directory prevents *all* configured filters from executing,
/// including clean/process filters invoked while comparing the worktree.
/// The real repository config is never edited. Missing promisor objects fail
/// locally instead of causing an implicit fetch. Submodule recursion is off.
async fn read_only_view(
    workspace: &Workspace,
    cancel: &CancellationToken,
    deadline: Instant,
) -> AgentResult<(tempfile::TempDir, Command)> {
    let paths = discover(
        workspace,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
            "--git-path",
            "index",
            "--show-object-format",
            "--shared-index-path",
        ],
        cancel,
        deadline,
        false,
    )
    .await?;
    let mut paths = paths.lines();
    let objects = paths.next().unwrap_or("");
    let index = paths.next().unwrap_or("");
    let format = paths.next().unwrap_or("");
    if !std::path::Path::new(objects).is_absolute()
        || !std::path::Path::new(index).is_absolute()
        || !matches!(format, "sha1" | "sha256")
    {
        return Err(AgentError::Tool("unsupported git repository layout".into()));
    }
    let shared_index = paths.next().filter(|p| !p.is_empty());
    if paths.next().is_some() {
        return Err(AgentError::Tool("ambiguous git repository paths".into()));
    }
    let head = discover(
        workspace,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        cancel,
        deadline,
        true,
    )
    .await?;
    let head = head.trim();
    if !head.is_empty()
        && (head.len() != if format == "sha1" { 40 } else { 64 }
            || !head.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(AgentError::Tool("invalid git HEAD identity".into()));
    }
    let settings = discover(
        workspace,
        &[
            "config",
            "--null",
            "--get-regexp",
            "^core\\.(filemode|symlinks|ignorecase|autocrlf|eol)$",
        ],
        cancel,
        deadline,
        true,
    )
    .await?;
    let view = tempfile::Builder::new()
        .prefix("focus-git-read-")
        .tempdir()
        .map_err(|e| AgentError::Tool(format!("create read-only git view: {e}")))?;
    let mut config = format!(
        "[core]\nrepositoryformatversion = {}\nbare = false\nfsmonitor = false\n",
        if format == "sha256" { 1 } else { 0 }
    );
    // A closed set of scalar values, never an include, path or command.
    for entry in settings.split('\0').filter(|e| !e.is_empty()) {
        let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
        let value = value.to_ascii_lowercase();
        let valid = match key {
            "core.filemode" | "core.symlinks" | "core.ignorecase" => matches!(
                value.as_str(),
                "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
            ),
            "core.autocrlf" => matches!(value.as_str(), "true" | "false" | "input"),
            "core.eol" => matches!(value.as_str(), "lf" | "crlf" | "native"),
            _ => false,
        };
        if !valid {
            return Err(AgentError::Tool("unsupported git worktree setting".into()));
        }
        config.push_str(&format!("{} = {value}\n", key.trim_start_matches("core.")));
    }
    if format == "sha256" {
        config.push_str("[extensions]\nobjectFormat = sha256\n");
    }
    let io = async {
        tokio::fs::create_dir(view.path().join("objects")).await?;
        tokio::fs::create_dir(view.path().join("refs")).await?;
        tokio::fs::write(view.path().join("config"), config).await?;
        tokio::fs::write(
            view.path().join("HEAD"),
            if head.is_empty() {
                "ref: refs/heads/read-only\n".to_string()
            } else {
                format!("{head}\n")
            },
        )
        .await?;
        if let Some(shared) = shared_index {
            let name = std::path::Path::new(shared)
                .file_name()
                .filter(|name| name.to_string_lossy().starts_with("sharedindex."))
                .ok_or_else(|| std::io::Error::other("invalid shared index path"))?;
            // Shared indexes are immutable, but the private directory may be
            // on another volume. Copy through a bounded reader, not a link.
            const MAX_SHARED_INDEX_BYTES: u64 = 64 * 1024 * 1024;
            let mut source = tokio::fs::File::open(shared)
                .await?
                .take(MAX_SHARED_INDEX_BYTES + 1);
            let mut target = tokio::fs::File::create(view.path().join(name)).await?;
            if tokio::io::copy(&mut source, &mut target).await? > MAX_SHARED_INDEX_BYTES {
                return Err(std::io::Error::other(
                    "shared index exceeds read-only view limit",
                ));
            }
            target.flush().await?;
        }
        Ok::<_, std::io::Error>(())
    }
    .await;
    io.map_err(|e| AgentError::Tool(format!("prepare read-only git view: {e}")))?;
    let mut command = git_command(workspace)?;
    command
        .arg("--git-dir")
        .arg(view.path())
        .arg("--work-tree")
        .arg(workspace.root())
        .env("GIT_OBJECT_DIRECTORY", objects)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_ATTR_NOSYSTEM", "1");
    Ok((view, command))
}

pub struct GitStatusTool {
    workspace: Workspace,
}

impl GitStatusTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

pub struct GitDiffTool {
    workspace: Workspace,
}

impl GitDiffTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct DiffArgs {
    /// Optional path filter; `--staged`-style flags are not accepted (kept read-only).
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    staged: bool,
}

async fn run_git(
    workspace: &Workspace,
    args: &[&str],
    run_id: RunId,
    call_id: &str,
    tool_name: &str,
    cancel: CancellationToken,
) -> AgentResult<ToolOutput> {
    let deadline = Instant::now() + Duration::from_millis(GIT_TIMEOUT_MS);
    let (_view, mut command) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(AgentError::Cancelled),
        result = timeout_at(deadline, read_only_view(workspace, &cancel, deadline)) => {
            result.map_err(|_| AgentError::Tool("git timed out".into()))??
        }
    };
    command.args(args);
    let (status, stdout, stderr) = capture_git(command, &cancel, deadline, MAX_PIPE_BYTES).await?;
    let total_bytes = stdout.total.saturating_add(stderr.total);
    let mut capture_truncated =
        stdout.total > stdout.bytes.len() || stderr.total > stderr.bytes.len();
    let stdout = String::from_utf8_lossy(&stdout.bytes);
    let stderr = String::from_utf8_lossy(&stderr.bytes);
    let combined = if stderr.trim().is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n[stderr]\n{stderr}")
    };
    let mut combined = strip_runtime_paths(&combined);
    // Lossy UTF-8 decoding can expand byte length. The artifact ceiling
    // applies to the actual encoded artifact, not just the input pipes.
    if combined.len() > super::stream::MAX_ARTIFACT_BYTES {
        let mut end = super::stream::MAX_ARTIFACT_BYTES;
        while !combined.is_char_boundary(end) {
            end -= 1;
        }
        combined.truncate(end);
        capture_truncated = true;
    }

    let ok = status.success();
    if !ok && combined.trim().is_empty() {
        return Ok(ToolOutput {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            ok: false,
            summary: format!("git {args:?} failed (exit={:?})", status.code()),
            model_content: "(empty output; is this a git repository?)".into(),
            artifact_ref: None,
            metadata: json!({"exit_code": status.code()}),
        }
        .with_native_execution_facts(super::builtin_bound(false)));
    }

    let truncated = capture_truncated || combined.chars().count() > MODEL_OUTPUT_CHARS;
    let bounded = tail_chars(&combined, MODEL_OUTPUT_CHARS);
    let artifact_ref = if truncated {
        let write = workspace.write_artifact(
            run_id,
            tool_name.trim_start_matches("git."),
            "txt",
            combined.as_bytes(),
        );
        Some(tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AgentError::Cancelled),
            result = timeout_at(deadline, write) => result.map_err(|_| AgentError::Tool("git output capture timed out".into()))??,
        })
    } else {
        None
    };

    Ok(ToolOutput {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        ok,
        summary: format!(
            "git {args:?} (exit={}, {} bytes)",
            status
                .code()
                .map_or_else(|| "signal".to_string(), |v| v.to_string()),
            combined.len()
        ),
        model_content: if truncated {
            format!(
                "{}\n\nCaptured output{}: {}",
                bounded,
                if capture_truncated { " (prefix truncated at byte limit)" } else { "" },
                artifact_ref.as_deref().unwrap_or("")
            )
        } else {
            bounded
        },
        artifact_ref,
        metadata: json!({"exit_code": status.code(), "bytes": total_bytes, "captured_bytes": combined.len(), "capture_truncated": capture_truncated, "external_filters_disabled": true}),
    }
    .with_native_execution_facts(super::builtin_bound(false)))
}

fn strip_runtime_paths(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let normalized = line.replace('\\', "/");
            !normalized.contains(".focus-agent")
                && !normalized.contains("/.git/")
                && normalized.trim() != ".git"
                && !normalized.trim().ends_with("/.git")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let skip = count - max_chars;
    format!(
        "...[{} chars omitted; showing tail]\n{}",
        skip,
        text.chars().skip(skip).collect::<String>()
    )
}

#[async_trait]
impl Tool for GitStatusTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.status".into(),
            description: "Show `git status --short` for the workspace (read-only).".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::InspectDiff],
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        _arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let output = run_git(
            &self.workspace,
            &[
                "status",
                "--short",
                "--ignore-submodules=all",
                "--",
                ".",
                ":(exclude).focus-agent",
                ":(exclude).focus-agent/**",
            ],
            run_id,
            call_id,
            "git.status",
            cancel,
        )
        .await?;
        Ok(ToolOutcome::Value(output))
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "git.diff".into(),
            description:
                "Show `git diff` (optionally --staged or a path) for the workspace (read-only)."
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "staged": {"type": "boolean"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::InspectDiff],
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: DiffArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("git.diff args: {e}")))?;
        let mut git_args: Vec<String> = vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
            "--ignore-submodules=all".into(),
        ];
        if args.staged {
            git_args.push("--staged".into());
        }
        if let Some(path) = args.path
            && !path.is_empty()
        {
            // Reject absolute/parent escapes and links that leave the
            // workspace. The explicit `--` then makes even an option-looking
            // filename a pathspec rather than a git option.
            self.workspace.resolve_relative(&path).await?;
            git_args.push("--".into());
            git_args.push(path);
        }
        if !git_args.iter().any(|arg| arg == "--") {
            git_args.push("--".into());
            git_args.push(".".into());
        }
        git_args.push(":(exclude).focus-agent".into());
        git_args.push(":(exclude).focus-agent/**".into());
        let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
        let output = run_git(&self.workspace, &refs, run_id, call_id, "git.diff", cancel).await?;
        Ok(ToolOutcome::Value(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn git_command(root: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git executable is required for git tool tests")
    }

    #[tokio::test]
    async fn readonly_tools_do_not_run_repository_helpers_or_refresh_the_index() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            git_command(dir.path(), &["init", "--quiet"])
                .status
                .success()
        );
        std::fs::write(dir.path().join("tracked.txt"), "before\n").unwrap();
        assert!(
            git_command(dir.path(), &["add", "tracked.txt"])
                .status
                .success()
        );
        std::fs::write(dir.path().join("tracked.txt"), "after\n").unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.txt diff=hostile filter=hostile\n",
        )
        .unwrap();
        for key in [
            "diff.external",
            "diff.hostile.command",
            "diff.hostile.textconv",
            "filter.hostile.clean",
            "filter.hostile.process",
            "core.fsmonitor",
        ] {
            assert!(
                git_command(
                    dir.path(),
                    &["config", key, "echo invoked > readonly-helper-ran"]
                )
                .status
                .success()
            );
        }
        assert!(
            git_command(dir.path(), &["config", "filter.hostile.required", "true"])
                .status
                .success()
        );
        let index_before = std::fs::read(dir.path().join(".git/index")).unwrap();
        let config_before = std::fs::read(dir.path().join(".git/config")).unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::write(
            dir.path().join("git.exe"),
            "workspace executable must not be selected",
        )
        .unwrap();
        for tool in [
            Box::new(GitDiffTool::new(workspace.clone())) as Box<dyn Tool>,
            Box::new(GitStatusTool::new(workspace)),
        ] {
            let ToolOutcome::Value(output) = tool
                .execute(
                    RunId::new(),
                    "read",
                    json!({}),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap()
            else {
                panic!("value");
            };
            assert!(output.ok, "{}", output.model_content);
            assert!(output.model_content.contains("tracked.txt"));
            assert!(
                !dir.path().join("readonly-helper-ran").exists(),
                "read-only Git launched a configured helper"
            );
        }
        assert_eq!(
            std::fs::read(dir.path().join(".git/index")).unwrap(),
            index_before
        );
        assert_eq!(
            std::fs::read(dir.path().join(".git/config")).unwrap(),
            config_before
        );
    }

    #[tokio::test]
    async fn diff_drains_output_larger_than_a_pipe_without_waiting_for_timeout() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            git_command(dir.path(), &["init", "--quiet"])
                .status
                .success()
        );
        std::fs::write(
            dir.path().join("tracked.txt"),
            "before xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n".repeat(16000),
        )
        .unwrap();
        assert!(
            git_command(dir.path(), &["add", "tracked.txt"])
                .status
                .success()
        );
        std::fs::write(
            dir.path().join("tracked.txt"),
            "after xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n".repeat(16000),
        )
        .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitDiffTool::new(workspace);
        let ToolOutcome::Value(output) = timeout(
            Duration::from_secs(10),
            tool.execute(
                RunId::new(),
                "large",
                json!({}),
                None,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("large diff must not deadlock")
        .unwrap() else {
            panic!("value");
        };
        assert!(output.ok, "{}", output.model_content);
        assert!(output.metadata["bytes"].as_u64().unwrap() > 500_000);
        assert_eq!(output.metadata["capture_truncated"], false);
        assert!(output.artifact_ref.is_some());
        assert!(output.model_content.chars().count() < MODEL_OUTPUT_CHARS + 500);
    }

    #[tokio::test]
    async fn capture_limit_still_drains_every_byte() {
        let (mut writer, reader) = tokio::io::duplex(32);
        let write = async move {
            use tokio::io::AsyncWriteExt;
            writer.write_all(&vec![b'x'; 20_000]).await.unwrap();
        };
        let (_, captured) = tokio::join!(write, drain_pipe(reader, 100));
        let captured = captured.unwrap();
        assert_eq!(captured.bytes.len(), 100);
        assert_eq!(captured.total, 20_000);
    }

    #[tokio::test]
    async fn diff_treats_option_looking_path_as_a_path_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let init = git_command(dir.path(), &["init", "--quiet"]);
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        tokio::fs::write(dir.path().join("tracked.txt"), "before\n")
            .await
            .unwrap();
        let add = git_command(dir.path(), &["add", "--", "tracked.txt"]);
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        tokio::fs::write(dir.path().join("tracked.txt"), "after\n")
            .await
            .unwrap();

        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitDiffTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "diff-call",
                json!({"path": "--output=leak.patch"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ToolOutcome::Value(output) if output.ok));
        assert!(
            !dir.path().join("leak.patch").exists(),
            "an option-looking path must not activate git's --output option"
        );
    }

    #[tokio::test]
    async fn private_view_preserves_staged_and_worktree_diffs_with_a_split_index() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            git_command(dir.path(), &["init", "--quiet"])
                .status
                .success()
        );
        assert!(
            git_command(dir.path(), &["config", "core.autocrlf", "false"])
                .status
                .success()
        );
        std::fs::write(dir.path().join("tracked.txt"), "before\n").unwrap();
        assert!(
            git_command(dir.path(), &["add", "tracked.txt"])
                .status
                .success()
        );
        assert!(
            git_command(dir.path(), &["update-index", "--split-index"])
                .status
                .success()
        );
        std::fs::write(dir.path().join("tracked.txt"), "after\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitDiffTool::new(workspace);
        for staged in [false, true] {
            let ToolOutcome::Value(output) = tool
                .execute(
                    RunId::new(),
                    "split",
                    json!({"staged": staged}),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap()
            else {
                panic!("value");
            };
            assert!(output.ok, "{}", output.model_content);
            assert!(
                output
                    .model_content
                    .contains(if staged { "+before" } else { "+after" }),
                "{}",
                output.model_content
            );
        }
    }

    #[tokio::test]
    async fn diff_rejects_paths_outside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitDiffTool::new(workspace);
        let result = tool
            .execute(
                RunId::new(),
                "diff-call",
                json!({"path": "../outside.txt"}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AgentError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn status_honors_preexisting_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = GitStatusTool::new(workspace);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = tool
            .execute(RunId::new(), "status-call", json!({}), None, cancel)
            .await;
        assert!(matches!(result, Err(AgentError::Cancelled)));
    }

    #[tokio::test]
    async fn status_hides_focus_agent_runtime_state() {
        let dir = tempfile::tempdir().unwrap();
        git_command(dir.path(), &["init", "--quiet"]);
        git_command(dir.path(), &["config", "core.autocrlf", "false"]);
        tokio::fs::write(dir.path().join("readme.txt"), "ok\n")
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        tokio::fs::write(workspace.state_dir().join("secret.jsonl"), "lease\n")
            .await
            .unwrap();
        let tool = GitStatusTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "status-call",
                json!({}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("expected a value");
        };
        assert!(
            !output.model_content.contains(".focus-agent"),
            "git.status leaked runtime state: {}",
            output.model_content
        );
    }
}
