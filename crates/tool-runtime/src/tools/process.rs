//! `process.run` — structured argv process execution.
//!
//! The TOOLS-06 alternative to the raw `shell.exec` string: the command is
//! an explicit argv vector, so there is no shell to parse (and no shell
//! injection to guard). cwd is a workspace-relative directory, env is an
//! explicit override map layered on the inherited environment, and the
//! timeout/cancel paths kill the whole process tree (not just the direct
//! child). Output streams into the same bounded tail + artifact shape as
//! `shell.exec`.

use std::{collections::HashMap, process::Stdio};

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, HostToolPolicy, OperationEffectContext, RunId,
    ToolOutcome, ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec, attach_failure_class,
};
use agent_process::kill_process_tree;
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::BufWriter, process::Command, sync::mpsc, time::Duration};

use super::Tool;
use super::content_digest;
use super::stream::{
    MAX_ARTIFACT_BYTES, StreamCapture, StreamChunk, spawn_stderr_reader, spawn_stdout_reader,
};

const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_ARGV: usize = 64;
const MAX_ARG_CHARS: usize = 16_384;
const MAX_ENV_KEYS: usize = 64;
const MAX_ENV_VALUE_CHARS: usize = 16_384;

// TOOL-PROC-01：argv0 的解析语义由 host 显式定义，不再依赖
// `Command::new(argv0)` + `current_dir` 的平台隐式行为。Windows 的
// CreateProcess 不搜索子进程 cwd——cwd 里确实存在的 binary 会被报
// not_found，直接诱发模型的 `foo` / `./foo` / `.\foo` 猜测循环。
//
// 规则（与 cmd.exe 直觉一致、对模型可见）：
// - 绝对路径：原样使用；
// - 带分隔符的相对路径（./x、.\x、sub/x）：相对本次调用的 cwd；
// - 裸名：先查 cwd，再按 effective PATH 顺序；Windows 下末段无扩展名
//   时依次尝试 PATHEXT 补全。
// spawn 一律使用解析出的绝对路径。preflight / RetryDomain 指纹 /
// spawn 三处共享同一份解释。
const RESOLVER_RULES_VERSION: &[u8] = b"program-resolver-v1";
/// 指纹目录状态的有界上限：条目数与缓冲字节双界，超限以截断标记入哈希。
const MAX_FINGERPRINT_ENTRIES: usize = 4096;
const MAX_FINGERPRINT_BUFFER_BYTES: usize = 128 * 1024;
const MAX_ATTEMPTED_CANDIDATES: usize = 8;

struct ProgramResolution {
    /// 传给 `Command::new` 的绝对路径。
    executable: std::path::PathBuf,
    /// 稳定 lineage scope（CONV-03）：digest(cwd 身份 + effective PATH
    /// + 规则版本)。目录内容变化不改变它。
    scope_key: String,
    /// 当前 epoch 前置指纹：完整有界目录状态 + PATH + 规范化 env 覆盖。
    /// build 产出 binary 会改变它；普通源码 edit 不变名字集时也不变。
    fingerprint: String,
}

struct ProgramResolutionFailure {
    candidates_tried: Vec<String>,
}

fn has_path_separator(argv0: &str) -> bool {
    argv0.contains('/') || argv0.contains('\\')
}

fn is_absolute(argv0: &str) -> bool {
    std::path::Path::new(argv0).is_absolute()
}

/// Windows 下末段无扩展名的候选按 PATHEXT 顺序补全；其余平台只做直查。
/// 返回第一个真实存在的具体路径。
fn candidate_executable(path: std::path::PathBuf) -> Option<std::path::PathBuf> {
    if path.is_file() {
        return Some(path);
    }
    #[cfg(windows)]
    {
        let has_extension = path.extension().is_some_and(|ext| !ext.is_empty());
        if !has_extension {
            let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            for ext in pathext.split(';') {
                let ext = ext.trim();
                if ext.is_empty() {
                    continue;
                }
                let ext = ext.strip_prefix('.').unwrap_or(ext);
                let mut candidate = path.as_os_str().to_owned();
                candidate.push(".");
                candidate.push(ext);
                let candidate = std::path::PathBuf::from(candidate);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn effective_path_dirs() -> Vec<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect()
}

/// 完整有界目录状态摘要：全部条目（含点文件）排序后逐个入哈希
/// （kind 字节 + 名字），条目数与缓冲字节双界，超限以截断标记收尾。
/// 显示给模型的是前 20 个名字的 preview；identity 绝不只哈希显示窗口
/// ——这与 fs.list 的修正同理。
fn directory_state_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut names: Vec<(bool, String)> = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if names.len() >= MAX_FINGERPRINT_ENTRIES {
            truncated = true;
            break;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        names.push((is_dir, entry.file_name().to_string_lossy().into_owned()));
    }
    names.sort();
    let mut buffer = Vec::with_capacity(256);
    for (is_dir, name) in &names {
        if buffer.len() >= MAX_FINGERPRINT_BUFFER_BYTES {
            truncated = true;
            break;
        }
        buffer
            .extend_from_slice(format!("{}{}\n", if *is_dir { 'd' } else { 'f' }, name).as_bytes());
    }
    buffer.extend_from_slice(
        format!(
            "entries={}truncated={}\n",
            names.len(),
            if truncated { 1 } else { 0 }
        )
        .as_bytes(),
    );
    buffer
}

fn canonical_env_bytes(env_overrides: &HashMap<String, String>) -> Vec<u8> {
    let mut pairs: Vec<(&String, &String)> = env_overrides.iter().collect();
    pairs.sort();
    let mut buffer = Vec::new();
    for (key, value) in pairs {
        buffer.extend_from_slice(key.as_bytes());
        buffer.extend_from_slice(b"=");
        buffer.extend_from_slice(value.as_bytes());
        buffer.push(0);
    }
    buffer
}

/// 解析身份二元组：(scope_key, fingerprint)。scope 只含 cwd 身份 +
/// PATH + 规则版本（跨 epoch 稳定）；fingerprint 另含完整目录状态与
/// 规范化 env（epoch 随世界变化）。HashMap 序列化顺序不稳定，env 一律
/// 先规范化排序再入哈希。
fn resolution_identity(
    cwd: &std::path::Path,
    env_overrides: &HashMap<String, String>,
) -> (String, String) {
    let path_text = std::env::var("PATH").unwrap_or_default();
    let scope_source: Vec<u8> = RESOLVER_RULES_VERSION
        .iter()
        .copied()
        .chain(cwd.to_string_lossy().as_bytes().iter().copied())
        .chain(b"\0PATH=".iter().copied())
        .chain(path_text.as_bytes().iter().copied())
        .collect();
    let mut fingerprint_source = directory_state_bytes(cwd);
    fingerprint_source.extend_from_slice(b"\0PATH=");
    fingerprint_source.extend_from_slice(path_text.as_bytes());
    fingerprint_source.push(0);
    fingerprint_source.extend_from_slice(&canonical_env_bytes(env_overrides));
    fingerprint_source.push(0);
    fingerprint_source.extend_from_slice(RESOLVER_RULES_VERSION);
    (
        content_digest(&scope_source),
        content_digest(&fingerprint_source),
    )
}

fn resolve_program(
    argv0: &str,
    cwd: &std::path::Path,
    env_overrides: &HashMap<String, String>,
) -> Result<ProgramResolution, ProgramResolutionFailure> {
    let mut attempted: Vec<String> = Vec::new();
    let try_candidate = |candidate: std::path::PathBuf, attempted: &mut Vec<String>| {
        if attempted.len() < MAX_ATTEMPTED_CANDIDATES {
            attempted.push(candidate.to_string_lossy().into_owned());
        }
        candidate_executable(candidate)
    };

    // 相对路径禁止 `..` 逃逸出本次调用的 cwd（cwd 本身已被工作区约束，
    // argv0 不能绕过这层围栏）。
    if !is_absolute(argv0) && has_path_separator(argv0) {
        let escapes = std::path::Path::new(argv0)
            .components()
            .any(|component| component == std::path::Component::ParentDir);
        if escapes {
            return Err(ProgramResolutionFailure {
                candidates_tried: Vec::new(),
            });
        }
    }

    let resolved = if is_absolute(argv0) {
        try_candidate(std::path::PathBuf::from(argv0), &mut attempted)
    } else if has_path_separator(argv0) {
        try_candidate(cwd.join(argv0), &mut attempted)
    } else {
        // 裸名：cwd 优先（cmd.exe 语义），再按 PATH 顺序。
        try_candidate(cwd.join(argv0), &mut attempted).or_else(|| {
            effective_path_dirs().into_iter().find_map(|dir| {
                let full = dir.join(argv0);
                if attempted.len() < MAX_ATTEMPTED_CANDIDATES {
                    attempted.push(full.to_string_lossy().into_owned());
                }
                candidate_executable(full)
            })
        })
    };

    match resolved {
        Some(executable) => {
            let (scope_key, fingerprint) = resolution_identity(cwd, env_overrides);
            Ok(ProgramResolution {
                executable,
                scope_key,
                fingerprint,
            })
        }
        None => Err(ProgramResolutionFailure {
            candidates_tried: attempted,
        }),
    }
}

/// Bounded host-side executable identity used by exact verification reuse.
/// It intentionally excludes general cwd contents (build output would change
/// those during a successful verifier) and includes only resolver version,
/// effective PATH, resolved executable path/metadata and explicit env.
pub(crate) fn verification_executable_identity(
    argv0: &str,
    cwd: &std::path::Path,
    env_overrides: &HashMap<String, String>,
) -> String {
    let mut source = Vec::new();
    source.extend_from_slice(RESOLVER_RULES_VERSION);
    source.extend_from_slice(b"\0PATH=");
    source.extend_from_slice(std::env::var("PATH").unwrap_or_default().as_bytes());
    source.push(0);
    source.extend_from_slice(&canonical_env_bytes(env_overrides));
    source.push(0);
    match resolve_program(argv0, cwd, env_overrides) {
        Ok(resolution) => {
            source.extend_from_slice(resolution.executable.to_string_lossy().as_bytes());
            if let Ok(metadata) = std::fs::metadata(&resolution.executable) {
                source.extend_from_slice(&metadata.len().to_le_bytes());
                if let Ok(modified) = metadata.modified()
                    && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
                {
                    source.extend_from_slice(&duration.as_secs().to_le_bytes());
                    source.extend_from_slice(&duration.subsec_nanos().to_le_bytes());
                }
            }
        }
        Err(failure) => {
            source.extend_from_slice(b"unresolved\0");
            for candidate in failure.candidates_tried {
                source.extend_from_slice(candidate.as_bytes());
                source.push(0);
            }
        }
    }
    content_digest(&source)
}

pub struct ProcessRunTool {
    workspace: Workspace,
}

impl ProcessRunTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
pub(crate) struct ProcessArgs {
    pub(crate) argv: Vec<String>,
    /// Workspace-relative working directory for the process (defaults to
    /// the workspace root).
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// Explicit environment overrides layered on the inherited environment.
    #[serde(default)]
    pub(crate) env: HashMap<String, String>,
    #[serde(default = "default_timeout_ms")]
    pub(crate) timeout_ms: u64,
}

/// One fully resolved process request. Keeping the authority inputs beside the
/// actual argv makes the common runner reusable without growing an unrelated
/// positional-argument list as trusted process tools are added.
pub(crate) struct ProcessInvocation<'a> {
    pub(crate) tool_name: &'a str,
    pub(crate) run_id: RunId,
    pub(crate) call_id: &'a str,
    pub(crate) authority_arguments: &'a Value,
    pub(crate) authority_policy: &'a HostToolPolicy,
    pub(crate) args: ProcessArgs,
    pub(crate) effect_context: Option<OperationEffectContext>,
    pub(crate) cancel: CancellationToken,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[async_trait]
impl Tool for ProcessRunTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "process.run".into(),
            description: "Run a program with an explicit argv (no shell), optionally in a workspace-relative cwd with explicit env overrides. Program resolution: absolute paths as-is; ./name, .\\name and sub/name resolve inside the cwd; a bare name searches the cwd first, then PATH (PATHEXT-aware on Windows). A bounded output prefix streams to an artifact; only a bounded tail reaches the model. Timeout/cancel kill the whole process tree.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["argv"],
                "properties": {
                    "argv": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "items": {"type": "string"},
                        "description": "Program and arguments, passed verbatim (no shell parsing)"
                    },
                    "cwd": {"type": "string", "description": "Workspace-relative working directory"},
                    "env": {"type": "object", "description": "Explicit environment overrides (layered on the inherited environment)"},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 120000}
                }
            }),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ProcessArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| AgentError::InvalidRequest(format!("process.run args: {e}")))?;
        let policy = crate::BUILTIN_TOOL_POLICIES
            .iter()
            .find(|policy| policy.tool_name == "process.run")
            .expect("process.run builtin policy must exist");
        self.execute_invocation(ProcessInvocation {
            tool_name: "process.run",
            run_id,
            call_id,
            authority_arguments: &arguments,
            authority_policy: policy,
            args,
            effect_context,
            cancel,
        })
        .await
    }
}

impl ProcessRunTool {
    pub(crate) async fn execute_invocation(
        &self,
        invocation: ProcessInvocation<'_>,
    ) -> AgentResult<ToolOutcome> {
        let ProcessInvocation {
            tool_name,
            run_id,
            call_id,
            authority_arguments,
            authority_policy,
            args,
            effect_context,
            cancel,
        } = invocation;
        if args.argv.is_empty() {
            return Err(AgentError::InvalidRequest(format!(
                "{tool_name} resolved to an empty argv"
            )));
        }
        if args.argv.len() > MAX_ARGV {
            return Err(AgentError::InvalidRequest(format!(
                "{tool_name} argv is limited to {MAX_ARGV} arguments"
            )));
        }
        if args
            .argv
            .iter()
            .any(|arg| arg.chars().count() > MAX_ARG_CHARS)
        {
            return Err(AgentError::InvalidRequest(format!(
                "{tool_name} argv arguments are limited to {MAX_ARG_CHARS} chars"
            )));
        }
        if args.env.len() > MAX_ENV_KEYS {
            return Err(AgentError::InvalidRequest(format!(
                "{tool_name} env is limited to {MAX_ENV_KEYS} keys"
            )));
        }
        if args
            .env
            .values()
            .any(|value| value.chars().count() > MAX_ENV_VALUE_CHARS)
        {
            return Err(AgentError::InvalidRequest(format!(
                "{tool_name} env values are limited to {MAX_ENV_VALUE_CHARS} chars"
            )));
        }
        let timeout_ms = args.timeout_ms.clamp(100, MAX_TIMEOUT_MS);

        // The cwd is confined to the workspace (lexical `..` rejection
        // lives in the workspace's path resolution); the default is the
        // workspace root.
        let cwd = match &args.cwd {
            Some(relative) => self.workspace.resolve_relative(relative).await?,
            None => self.workspace.root().to_path_buf(),
        };

        super::require_process_effect_context(&effect_context, tool_name)?;
        let actual_intent = agent_contracts::exec_argv_intent(&args.argv);
        if !authority_policy
            .intent_from(authority_arguments)
            .covers(&actual_intent)
        {
            return Err(AgentError::InvalidRequest(
                "actual process command is not covered by the approved effect intent; the child was not started".into(),
            ));
        }

        // TOOL-PROC-01：preflight 显式解析。失败在这里以类型化输出返回
        // （附尝试过的候选与完整身份指纹），不再把隐式语义留给 spawn。
        let resolution = match resolve_program(&args.argv[0], &cwd, &args.env) {
            Ok(resolution) => resolution,
            Err(failure) => {
                // 模型反复猜测不存在的程序名是实测最高频的失败循环：
                // 一次错误必须足以纠正。preview 只列前 20 个名字，
                // 身份指纹则覆盖完整有界目录状态（PROTO-EVID-03 同理）。
                let entries = bounded_cwd_listing(&cwd);
                let (scope_key, fingerprint) = resolution_identity(&cwd, &args.env);
                return Ok(ToolOutcome::Value(agent_contracts::tool_failure_output(
                    call_id,
                    tool_name,
                    agent_contracts::ToolFailureClass::PathNotFound,
                    format!("{tool_name} refused: program_not_found ({})", args.argv[0]),
                    format!(
                        "program `{}` was not found.\ncwd `{}` contains: {}\ntried: {}\ncompile or install it first, or run a binary that exists in the listing.",
                        args.argv[0],
                        cwd.display(),
                        if entries.is_empty() {
                            "(empty)".into()
                        } else {
                            entries.join(", ")
                        },
                        if failure.candidates_tried.is_empty() {
                            "(nothing: argv[0] may not traverse outside cwd)".into()
                        } else {
                            failure.candidates_tried.join(", ")
                        }
                    ),
                    json!({
                        "argv0": args.argv[0],
                        "cwd": cwd.display().to_string(),
                        "entries": entries,
                        "attempted": failure.candidates_tried,
                        "resolution_scope_key": scope_key,
                        "resolution_fingerprint": fingerprint,
                        "recovery_hint": "use a program that exists in the listing; compile sources before running their binaries",
                    }),
                )));
            }
        };

        let mut command = Command::new(&resolution.executable);
        command
            .args(&args.argv[1..])
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &args.env {
            command.env(key, value);
        }

        // Make the process a process-group leader on Unix so a
        // cancellation or timeout can kill its whole tree (`kill(-pgid)`),
        // not just the direct child.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // preflight 与 spawn 之间目标被删除（或平台补全竞态）：
                // 仍走同一条类型化路径，身份取 preflight 时盖章的值。
                let entries = bounded_cwd_listing(&cwd);
                return Ok(ToolOutcome::Value(agent_contracts::tool_failure_output(
                    call_id,
                    tool_name,
                    agent_contracts::ToolFailureClass::PathNotFound,
                    format!("{tool_name} refused: program_not_found ({})", args.argv[0]),
                    format!(
                        "program `{}` disappeared before spawn.\ncwd `{}` contains: {}",
                        args.argv[0],
                        cwd.display(),
                        if entries.is_empty() {
                            "(empty)".into()
                        } else {
                            entries.join(", ")
                        }
                    ),
                    json!({
                        "argv0": args.argv[0],
                        "cwd": cwd.display().to_string(),
                        "entries": entries,
                        "resolution_scope_key": resolution.scope_key,
                        "resolution_fingerprint": resolution.fingerprint,
                        "recovery_hint": "rebuild or reinstall the binary before running it",
                    }),
                )));
            }
            Err(e) => return Err(AgentError::Tool(format!("spawn {}: {e}", args.argv[0]))),
        };
        let pid = match super::persist_spawned_process(
            &self.workspace,
            &effect_context,
            &child,
            tool_name,
        ) {
            Ok(pid) => pid,
            Err(error) => {
                super::abandon_spawned_process(&mut child);
                let _ = child.kill().await;
                return Err(error);
            }
        };

        let mut artifact = BufWriter::new(
            self.workspace
                .create_artifact(run_id, "process", "log")
                .await?,
        );

        // Two fixed-buffer readers push bounded line fragments into one
        // bounded channel; a missing newline can never grow an allocation.
        let (line_tx, mut line_rx) = mpsc::channel::<StreamChunk>(512);
        if let Some(stdout) = child.stdout.take() {
            spawn_stdout_reader(stdout, line_tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_reader(stderr, line_tx.clone());
        }
        drop(line_tx);

        let mut capture = StreamCapture::new();

        let deadline = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(deadline);
        // After the process exits, keep draining pipe remnants for a short
        // grace window (background children can hold the pipe open).
        let grace = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(grace);

        let mut exited: Option<std::process::ExitStatus> = None;
        let mut grace_started = false;
        let mut outcome: &str = "completed";

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // Kill the whole process tree, not just the direct
                    // child: a descendant that outlives the cancel is an
                    // avoidable stale mutation.
                    kill_process_tree(child.id().unwrap_or(0));
                    let _ = child.kill().await;
                    outcome = "cancelled";
                    break;
                }
                _ = &mut deadline => {
                    kill_process_tree(child.id().unwrap_or(0));
                    let _ = child.kill().await;
                    outcome = "timed out";
                    break;
                }
                status = child.wait(), if exited.is_none() => {
                    exited = Some(status.map_err(|e| AgentError::Tool(format!("wait: {e}")))?);
                    grace_started = true;
                }
                _ = &mut grace, if grace_started => break,
                line = line_rx.recv() => {
                    match line {
                        Some(line) => {
                            capture.record(line, &mut artifact).await?;
                        }
                        None => {
                            if exited.is_none() {
                                exited = Some(child.wait().await.map_err(|e| AgentError::Tool(format!("wait: {e}")))?);
                            }
                            break;
                        }
                    }
                }
            }
        }
        if exited.is_none() {
            exited = Some(
                child
                    .wait()
                    .await
                    .map_err(|e| AgentError::Tool(format!("wait: {e}")))?,
            );
        }
        super::persist_process_exit(
            &self.workspace,
            pid,
            exited.as_ref().and_then(|status| status.code()),
        )?;
        let model_content = capture.model_tail();
        let total_lines = capture.total_lines();
        let total_bytes = capture.total_bytes();
        let artifact_bytes = capture.artifact_bytes();
        let artifact_truncated = capture.artifact_truncated();
        let sealed_artifact_ref = self.workspace.seal_buffered_artifact(artifact).await?;
        // An empty sealed capture is not useful evidence. Publishing its
        // locator invites a follow-up artifact.read that can only return zero
        // lines. Keep the capture/seal path uniform for durability and expose
        // a locator only when there are bytes to retrieve.
        let artifact_ref = (artifact_bytes > 0).then_some(sealed_artifact_ref);

        let exit_code = exited.as_ref().and_then(|status| status.code());
        let ok = outcome == "completed" && exited.as_ref().is_some_and(|s| s.success());
        let exit_text = exit_code.map(|v| v.to_string()).unwrap_or_else(|| {
            if outcome == "completed" {
                "signal".into()
            } else {
                outcome.into()
            }
        });
        let cwd_text = cwd
            .strip_prefix(self.workspace.root())
            .unwrap_or(&cwd)
            .to_string_lossy()
            .replace('\\', "/");
        let artifact_note = match (artifact_truncated, artifact_ref.as_deref()) {
            (true, Some(reference)) => Some(format!(
                "Artifact capture truncated at {MAX_ARTIFACT_BYTES} bytes; remaining output was drained but not stored. Captured prefix: {reference}"
            )),
            (false, Some(reference)) => Some(format!("Full output: {reference}")),
            (_, None) => None,
        };
        let truncation_summary = if artifact_truncated {
            ", artifact truncated"
        } else {
            ""
        };

        let argv_text = args.argv.join(" ");
        let mut metadata = json!({
            "exit_code": exit_code,
            "timeout_ms": timeout_ms,
            "lines": total_lines,
            "output_bytes": total_bytes,
            "artifact_bytes": artifact_bytes,
            "artifact_limit_bytes": MAX_ARTIFACT_BYTES,
            "artifact_truncated": artifact_truncated,
            "outcome": outcome,
            "cwd": if cwd_text.is_empty() { "." } else { &cwd_text },
            "argv": argv_text,
            // CONV-03 matched-success：成功也携带解析身份，义务账本只
            // 在 scope 与指纹都匹配时才认定 blocker 被真正解决。
            "resolution_scope_key": resolution.scope_key,
            "resolution_fingerprint": resolution.fingerprint,
        });
        if let Some(class) = super::classify_process_outcome(
            outcome,
            ok,
            &model_content,
            Some(&argv_text),
            None,
            &self.workspace.project_markers(),
        ) {
            attach_failure_class(&mut metadata, class);
        }

        let model_content = match (model_content.is_empty(), artifact_note) {
            (true, None) => "process produced no stdout/stderr".to_string(),
            (_, Some(note)) => format!("{model_content}\n\n{note}"),
            (false, None) => model_content,
        };
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            ok,
            summary: format!(
                "process {outcome} (exit={exit_text}, {total_lines} lines, ~{} KB output, ~{} KB captured{truncation_summary})",
                total_bytes / 1024,
                artifact_bytes / 1024,
            ),
            model_content,
            artifact_ref,
            metadata,
        }))
    }
}

/// Bounded, deterministic listing of the spawn cwd for the
/// program-not-found failure envelope. Dot-entries (.git,
/// .focus-agent) are skipped to match the fs.list conventions.
fn bounded_cwd_listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (!name.starts_with('.')).then_some(name)
        })
        .collect();
    names.sort();
    names.truncate(20);
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{EffectReconciler, EffectReconciliation};
    use serde_json::json;

    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("process.run must return a plain value"),
        }
    }

    /// An argv that echoes its argument; platform-independent.
    fn echo_argv(text: &str) -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "echo".into(), text.into()]
        }
        #[cfg(not(windows))]
        {
            vec!["echo".into(), text.into()]
        }
    }

    /// A successful argv with no stdout/stderr on either supported platform.
    fn quiet_success_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec![
                "cmd".into(),
                "/D".into(),
                "/S".into(),
                "/C".into(),
                "exit /b 0".into(),
            ]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), ":".into()]
        }
    }

    fn ctx(run_id: RunId, arguments: &Value) -> agent_contracts::OperationEffectContext {
        crate::tools::test_process_effect_context(run_id, "c", "process.run", arguments)
    }

    /// TOOL-PROC-01 回归：cwd 里真实存在的可执行文件，裸名 / `.\` /
    /// `./` 三种写法都必须能跑（Windows CreateProcess 不搜子进程 cwd
    /// 的平台行为被 resolver 显式抹平），绝对路径照常；不存在的名字
    /// 返回类型化失败并附尝试过的候选。
    #[tokio::test]
    async fn resolution_forms_for_a_binary_in_cwd_all_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        #[cfg(windows)]
        let src = std::path::PathBuf::from(
            std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into()),
        );
        #[cfg(not(windows))]
        let src = std::path::PathBuf::from("/bin/echo");
        assert!(src.is_file(), "source exe missing: {}", src.display());
        let program_name = if cfg!(windows) { "probe.exe" } else { "probe" };
        std::fs::copy(&src, dir.path().join(program_name)).unwrap();

        let tool = ProcessRunTool::new(workspace.clone());
        for form in [
            program_name.to_string(),
            format!(".\\{program_name}"),
            format!("./{program_name}"),
            dir.path().join(program_name).to_string_lossy().to_string(),
        ] {
            let run_id = RunId::new();
            let arguments = json!({"argv": [form, "hi"], "cwd": "."});
            let context = ctx(run_id, &arguments);
            let output = tool
                .execute(
                    run_id,
                    "c",
                    arguments,
                    Some(context),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let output = value(output);
            assert!(
                output.ok,
                "argv0 [{form}] must resolve inside cwd: {}",
                output.summary
            );
            assert!(
                output.metadata["resolution_scope_key"].is_string()
                    && !output.metadata["resolution_scope_key"]
                        .as_str()
                        .unwrap()
                        .is_empty(),
                "success metadata carries the resolution identity"
            );
        }

        // 不存在的名字：类型化失败，候选列表里能看到 resolver 试过的路径。
        let run_id = RunId::new();
        let arguments = json!({"argv": ["nope.exe"], "cwd": "."});
        let context = ctx(run_id, &arguments);
        let output = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(!output.ok);
        let attempted: Vec<String> =
            serde_json::from_value(output.metadata["attempted"].clone()).unwrap();
        assert!(
            attempted.iter().any(|candidate| candidate.contains("nope")),
            "tried candidates are reported: {attempted:?}"
        );
        // 失败与成功使用同一套身份语义：scope 稳定，指纹随目录状态变化。
        assert!(output.metadata["resolution_scope_key"].is_string());
        assert!(output.metadata["resolution_fingerprint"].is_string());
    }

    /// 指纹 v2：目录状态超过 preview 的 20 个名字后，第 25 个条目变化
    /// 也必须改变指纹（identity ≠ 显示窗口）；env 顺序不影响指纹；
    /// scope 在内容变化时保持稳定。
    #[test]
    fn fingerprint_covers_full_directory_state_and_canonical_env() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..30 {
            std::fs::write(dir.path().join(format!("file{index:02}.txt")), "x").unwrap();
        }
        let env_a: HashMap<String, String> =
            [("K1".into(), "v1".into()), ("K2".into(), "v2".into())].into();
        let (scope_a, fp_a) = resolution_identity(dir.path(), &env_a);

        // 同一状态：指纹稳定（HashMap 迭代顺序无关）。
        let env_b: HashMap<String, String> =
            [("K2".into(), "v2".into()), ("K1".into(), "v1".into())].into();
        let (scope_b, fp_b) = resolution_identity(dir.path(), &env_b);
        assert_eq!(scope_a, scope_b);
        assert_eq!(fp_a, fp_b);

        // 第 25 个条目消失：preview 前 20 名不变，但指纹必须变。
        // （目录状态指纹覆盖名字集合——解析域的前置是“存在与否”，
        // 同名文件的字节重建不属于解析前置。）
        std::fs::remove_file(dir.path().join("file24.txt")).unwrap();
        let (_scope_c, fp_c) = resolution_identity(dir.path(), &env_a);
        assert_ne!(fp_a, fp_c, "beyond-preview changes must move the epoch");
        assert_eq!(scope_a, _scope_c, "content changes never move the scope");

        // 新增文件同样推进 epoch，scope 不变。
        std::fs::write(dir.path().join("zz_new.bin"), "y").unwrap();
        let (_scope_d, fp_d) = resolution_identity(dir.path(), &env_a);
        assert_ne!(fp_c, fp_d);
        assert_eq!(scope_a, _scope_d);
    }

    #[tokio::test]
    async fn process_run_executes_argv_without_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();
        let arguments = json!({"argv": echo_argv("argv no shell")});
        let context = ctx(run_id, &arguments);
        let output = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok, "command failed: {}", output.summary);
        assert!(
            output.model_content.contains("argv no shell"),
            "the arg must arrive verbatim: {}",
            output.model_content
        );
        assert_eq!(output.metadata["cwd"], ".");
    }

    #[tokio::test]
    async fn empty_process_capture_does_not_publish_an_unreadable_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace);
        let run_id = RunId::new();
        let arguments = json!({"argv": quiet_success_argv()});
        let context = ctx(run_id, &arguments);
        let output = value(
            tool.execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );

        assert!(output.ok, "quiet command failed: {}", output.summary);
        assert_eq!(output.metadata["output_bytes"], 0);
        assert_eq!(output.metadata["artifact_bytes"], 0);
        assert!(output.artifact_ref.is_none());
        assert_eq!(
            output.model_content, "process produced no stdout/stderr",
            "zero-byte success must be terminal without inviting artifact.read"
        );
    }

    fn write_marker_argv() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "echo spawned> marker.txt".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "echo spawned > marker.txt".into()]
        }
    }

    #[tokio::test]
    async fn process_run_missing_program_lists_the_cwd_instead_of_a_dead_end() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::write(dir.path().join("src.rs"), "fn main() {}\n").unwrap();
        let tool = ProcessRunTool::new(workspace);
        let run_id = RunId::new();
        let arguments = json!({"argv": ["protocol_tests.exe"]});
        let context = ctx(run_id, &arguments);
        let output = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(!output.ok, "a missing program must fail the call");
        assert_eq!(
            output.metadata[agent_contracts::TOOL_FAILURE_CLASS_KEY],
            json!("path_not_found")
        );
        assert!(
            output.model_content.contains("protocol_tests.exe"),
            "the invented name must be echoed: {}",
            output.model_content
        );
        assert!(
            output.model_content.contains("src.rs"),
            "the cwd listing must show what exists: {}",
            output.model_content
        );
        assert_eq!(output.metadata["entries"], json!(["src.rs"]));
    }

    #[tokio::test]
    async fn process_run_without_effect_identity_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace);
        let marker = dir.path().join("marker.txt");
        let error = tool
            .execute(
                RunId::new(),
                "c",
                json!({"argv": write_marker_argv()}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot spawn without Core-issued effect identity"),
            "{error}"
        );
        assert!(
            !marker.exists(),
            "fail-closed admission must happen before the child can mutate"
        );
    }

    #[tokio::test]
    async fn process_run_rejects_a_mismatched_identity_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace);
        let marker = dir.path().join("marker.txt");
        let run_id = RunId::new();
        let arguments = json!({"argv": write_marker_argv()});
        let stolen =
            crate::tools::test_process_effect_context(run_id, "c", "shell.exec", &arguments);
        let error = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(stolen),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("process spawn identity is for 'shell.exec'"),
            "{error}"
        );
        assert!(
            !marker.exists(),
            "a shell.exec lease must not start a process.run child"
        );
    }

    #[tokio::test]
    async fn process_run_ignores_an_unused_command_field() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();
        let arguments = json!({
            "command": "this must not run",
            "argv": echo_argv("argv wins")
        });
        let context = ctx(run_id, &arguments);
        let output = value(
            tool.execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
        assert!(output.ok, "command failed: {}", output.summary);
        assert!(
            output.model_content.contains("argv wins"),
            "spawn must follow argv, not an unused command field: {}",
            output.model_content
        );
        assert!(
            !output.model_content.contains("this must not run"),
            "the unused command field must not be executed: {}",
            output.model_content
        );
    }

    #[tokio::test]
    async fn process_run_honors_cwd_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();

        // Print the cwd and an env override through the platform echo.
        #[cfg(windows)]
        let argv: Vec<String> = vec![
            "cmd".into(),
            "/C".into(),
            "echo %CD% && echo %TOOLS_06_VAR%".into(),
        ];
        #[cfg(not(windows))]
        let argv: Vec<String> = vec!["sh".into(), "-c".into(), "pwd && echo $TOOLS_06_VAR".into()];

        let arguments = json!({
            "argv": argv,
            "cwd": "sub",
            "env": {"TOOLS_06_VAR": "injected"},
            "timeout_ms": 15000
        });
        let context = ctx(run_id, &arguments);
        let output = tool
            .execute(
                run_id,
                "c",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output = value(output);
        assert!(output.ok, "command failed: {}", output.summary);
        assert_eq!(output.metadata["cwd"], "sub");
        assert!(
            output.model_content.contains("injected"),
            "the env override must reach the process: {}",
            output.model_content
        );
    }

    #[tokio::test]
    async fn process_run_rejects_escaping_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let run_id = RunId::new();
        let result = tool
            .execute(
                run_id,
                "c",
                json!({"argv": echo_argv("x"), "cwd": "../escape"}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(
            result.is_err(),
            "a cwd escaping the workspace must be refused"
        );
    }

    #[tokio::test]
    async fn process_run_cancellation_kills_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());

        #[cfg(windows)]
        let argv = vec![
            "ping".to_string(),
            "-n".to_string(),
            "20".to_string(),
            "127.0.0.1".to_string(),
        ];
        #[cfg(not(windows))]
        let argv = vec!["sleep".to_string(), "30".to_string()];

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let run_id = RunId::new();
        let arguments = json!({"argv": argv, "timeout_ms": 60000});
        let context = ctx(run_id, &arguments);
        let handle = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let output = tool
                .execute(run_id, "c", arguments, Some(context), cancel_for_task)
                .await
                .unwrap();
            (output, started.elapsed())
        });

        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel.cancel();

        let (output, elapsed) = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("tool did not stop after cancellation")
            .unwrap();
        let output = value(output);
        assert!(!output.ok, "cancelled process must report failure");
        assert!(
            output.summary.contains("cancel"),
            "summary should mention cancellation: {}",
            output.summary
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "cancellation took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_process_run_does_not_roll_back_a_file_the_child_already_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = ProcessRunTool::new(workspace.clone());
        let marker = dir.path().join("landed.txt");

        #[cfg(windows)]
        let argv: Vec<String> = vec![
            "cmd".into(),
            "/C".into(),
            "echo landed> landed.txt & ping -n 20 127.0.0.1".into(),
        ];
        #[cfg(not(windows))]
        let argv: Vec<String> = vec![
            "sh".into(),
            "-c".into(),
            "echo landed > landed.txt; sleep 30".into(),
        ];

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let run_id = RunId::new();
        let arguments = json!({"argv": argv, "timeout_ms": 60000});
        let context = ctx(run_id, &arguments);
        let reconcile_context = context.clone();
        let handle = tokio::spawn(async move {
            tool.execute(run_id, "c", arguments, Some(context), cancel_for_task)
                .await
                .unwrap()
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        cancel.cancel();
        assert!(
            marker.exists(),
            "the child must write the file before cancellation"
        );

        let output = value(
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("tool did not stop after cancellation")
                .unwrap(),
        );
        assert!(!output.ok, "cancelled process must report failure");
        assert!(
            marker.exists(),
            "cancellation kills the tree; it does not roll back mutations the child already performed"
        );
        match workspace.reconcile(&reconcile_context).unwrap() {
            EffectReconciliation::NotApplied { .. } => {
                panic!("a spawned process must not look like it never started")
            }
            EffectReconciliation::NotManaged => {
                panic!("process.run is a managed non-transactional process effect")
            }
            EffectReconciliation::CompletedValue { .. }
            | EffectReconciliation::Ambiguous { .. }
            | EffectReconciliation::Applied { .. } => {}
        }
    }
}
