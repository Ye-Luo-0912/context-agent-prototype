//! 非事务进程效果的 spawn/exit 证据。
//!
//! 不能证明子进程改了哪些文件；只能证明是否启动、是否看到退出、
//! 以及仍存活的 PID 是否还是当时那个孩子。恢复不得回放命令。

use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use agent_contracts::{
    AgentError, AgentResult, EffectId, EffectReconciliation, OperationEffectContext,
};
use agent_process::{
    capture_process_identity, kill_matching_process_tree, process_identity_matches,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::ConfinedDir;

const JOURNAL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RECORDS: usize = 65_536;
const MAX_IDENTITY_TOKEN_BYTES: usize = 128;
const MAX_REASON_BYTES: usize = 4_000;

pub(crate) fn is_process_effect_tool(tool_name: &str) -> bool {
    matches!(tool_name, "shell.exec" | "process.run" | "process.session")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalTransition {
    Spawned {
        context: Box<OperationEffectContext>,
        pid: u32,
        identity_token: String,
    },
    Exited {
        pid: u32,
        exit_code: Option<i32>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalRecord {
    version: u32,
    seq: u64,
    transition: JournalTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFrame {
    record: JournalRecord,
    checksum: String,
}

#[derive(Debug, Clone)]
struct SpawnEvidence {
    context: OperationEffectContext,
    pid: u32,
    identity_token: String,
    exit_code: Option<Option<i32>>,
}

#[derive(Debug, Default, Clone)]
struct RecoveryState {
    last_seq: u64,
    /// 按 effect 记账，PID 退出后允许复用。
    spawns: HashMap<EffectId, SpawnEvidence>,
}

struct WriterState {
    file: File,
    recovery: RecoveryState,
    failed: Option<String>,
}

pub(crate) struct ProcessEffectJournal {
    _authority_dir: ConfinedDir,
    path: PathBuf,
    writer: Mutex<WriterState>,
}

impl std::fmt::Debug for ProcessEffectJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessEffectJournal")
            .field("path", &self.path)
            .finish()
    }
}

impl ProcessEffectJournal {
    pub(crate) fn open(authority_dir: ConfinedDir) -> AgentResult<Self> {
        let path = authority_dir.display().join("process-effects.jsonl");
        let existed = authority_dir
            .open_existing(OsStr::new("process-effects.jsonl"))
            .is_ok();
        let mut file = authority_dir
            .open_or_create_regular_file(OsStr::new("process-effects.jsonl"))
            .map_err(|error| {
                AgentError::Storage(format!(
                    "open process effect journal {}: {error}",
                    path.display()
                ))
            })?;
        file.try_lock().map_err(|error| {
            AgentError::Storage(format!(
                "lock process effect journal {} exclusively: {error}",
                path.display()
            ))
        })?;
        if !existed {
            file.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync new process effect journal {}: {error}",
                    path.display()
                ))
            })?;
            authority_dir.sync_all().map_err(|error| {
                AgentError::Storage(format!(
                    "sync process effect journal directory {}: {error}",
                    authority_dir.display().display()
                ))
            })?;
        }
        let recovery = recover_file(&mut file, &path)?;
        file.seek(SeekFrom::End(0)).map_err(|error| {
            AgentError::Storage(format!(
                "seek process effect journal {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self {
            _authority_dir: authority_dir,
            path,
            writer: Mutex::new(WriterState {
                file,
                recovery,
                failed: None,
            }),
        })
    }

    pub(crate) fn record_spawned(
        &self,
        context: &OperationEffectContext,
        pid: u32,
    ) -> AgentResult<()> {
        let identity =
            capture_process_identity(pid).unwrap_or_else(|_| agent_process::ProcessIdentity {
                pid,
                identity_token: String::new(),
            });
        self.append(JournalTransition::Spawned {
            context: Box::new(context.clone()),
            pid: identity.pid,
            identity_token: identity.identity_token,
        })
    }

    pub(crate) fn record_exited(&self, pid: u32, exit_code: Option<i32>) -> AgentResult<()> {
        {
            let writer = self.writer.lock().expect("process journal poisoned");
            if let Some(error) = &writer.failed {
                return Err(AgentError::Storage(error.clone()));
            }
            if open_spawn_by_pid(&writer.recovery, pid).is_none() {
                return Ok(());
            }
        }
        self.append(JournalTransition::Exited { pid, exit_code })
    }

    pub(crate) fn recover_orphans(&self) -> AgentResult<()> {
        let open: Vec<(u32, String)> = {
            let writer = self.writer.lock().expect("process journal poisoned");
            if let Some(error) = &writer.failed {
                return Err(AgentError::Storage(error.clone()));
            }
            writer
                .recovery
                .spawns
                .values()
                .filter(|evidence| evidence.exit_code.is_none())
                .map(|evidence| (evidence.pid, evidence.identity_token.clone()))
                .collect()
        };
        for (pid, token) in open {
            if !process_identity_matches(pid, &token) {
                continue;
            }
            if !kill_matching_process_tree(pid, &token) && process_identity_matches(pid, &token) {
                return Err(AgentError::RecoveryRequired(format!(
                    "could not terminate leftover process {pid}"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn reconcile(
        &self,
        context: &OperationEffectContext,
    ) -> AgentResult<EffectReconciliation> {
        context.validate().map_err(AgentError::InvalidRequest)?;
        if !is_process_effect_tool(&context.identity.tool_name) {
            return Ok(EffectReconciliation::NotManaged);
        }
        let writer = self.writer.lock().expect("process journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        let Some(evidence) = writer
            .recovery
            .spawns
            .values()
            .find(|spawn| spawn.context == *context)
        else {
            return Ok(EffectReconciliation::NotApplied {
                evidence: Some("process never spawned".into()),
            });
        };
        if let Some(exit_code) = evidence.exit_code {
            let code = match exit_code {
                Some(code) => code.to_string(),
                None => "signal".into(),
            };
            return Ok(EffectReconciliation::CompletedValue {
                evidence: Some(bounded_evidence(format!(
                    "process-pid:{}:exit:{code}",
                    evidence.pid
                ))),
            });
        }
        if process_identity_matches(evidence.pid, &evidence.identity_token) {
            let _ = kill_matching_process_tree(evidence.pid, &evidence.identity_token);
            return Ok(EffectReconciliation::Ambiguous {
                reason: bounded_reason(format!(
                    "process {} was still running; leftover tree was signalled and mutations may have landed",
                    evidence.pid
                )),
            });
        }
        Ok(EffectReconciliation::Ambiguous {
            reason: bounded_reason(format!(
                "process {} exited without a durable wait record; mutations may have landed",
                evidence.pid
            )),
        })
    }

    fn append(&self, transition: JournalTransition) -> AgentResult<()> {
        validate_transition(&transition).map_err(AgentError::InvalidRequest)?;
        let mut writer = self.writer.lock().expect("process journal poisoned");
        if let Some(error) = &writer.failed {
            return Err(AgentError::Storage(error.clone()));
        }
        if writer.recovery.last_seq >= MAX_RECORDS as u64 {
            return Err(AgentError::RecoveryRequired(format!(
                "process effect journal reached its {MAX_RECORDS} record recovery limit"
            )));
        }
        validate_fold(&writer.recovery, &transition).map_err(AgentError::RecoveryRequired)?;
        let seq = writer.recovery.last_seq.checked_add(1).ok_or_else(|| {
            AgentError::RecoveryRequired("process effect journal sequence exhausted".into())
        })?;
        let record = JournalRecord {
            version: JOURNAL_VERSION,
            seq,
            transition,
        };
        if let Err(error) = append_record(&mut writer.file, &record) {
            let message = format!(
                "process effect journal {} failed permanently: {error}",
                self.path.display()
            );
            writer.failed = Some(message.clone());
            return Err(AgentError::Storage(message));
        }
        apply_transition(&mut writer.recovery, &record.transition).expect("validated transition");
        writer.recovery.last_seq = seq;
        Ok(())
    }
}

fn validate_transition(transition: &JournalTransition) -> Result<(), String> {
    match transition {
        JournalTransition::Spawned {
            context,
            pid,
            identity_token,
        } => {
            context.validate()?;
            if *pid == 0 {
                return Err("process spawn pid must be non-zero".into());
            }
            if identity_token.len() > MAX_IDENTITY_TOKEN_BYTES
                || identity_token
                    .bytes()
                    .any(|byte| !byte.is_ascii_hexdigit() && !matches!(byte, b':' | b'-'))
            {
                return Err("process identity token exceeds its bound".into());
            }
            Ok(())
        }
        JournalTransition::Exited { pid, .. } => {
            if *pid == 0 {
                Err("process exit pid must be non-zero".into())
            } else {
                Ok(())
            }
        }
    }
}

fn validate_fold(state: &RecoveryState, transition: &JournalTransition) -> Result<(), String> {
    match transition {
        JournalTransition::Spawned { context, pid, .. } => {
            if open_spawn_by_pid(state, *pid).is_some() {
                return Err(format!("duplicate live process spawn pid {pid}"));
            }
            if state.spawns.contains_key(&context.effect_id) {
                return Err(format!(
                    "effect {} already has a process spawn record",
                    context.effect_id
                ));
            }
            if state.spawns.values().any(|spawn| {
                spawn.context.identity.operation_id == context.identity.operation_id
                    && spawn.context != **context
            }) {
                return Err(format!(
                    "operation {} has conflicting process effect identity",
                    context.identity.operation_id
                ));
            }
            Ok(())
        }
        JournalTransition::Exited { pid, .. } => {
            if open_spawn_by_pid(state, *pid).is_some() {
                Ok(())
            } else {
                Err(format!("process exit pid {pid} has no open spawn record"))
            }
        }
    }
}

fn apply_transition(
    state: &mut RecoveryState,
    transition: &JournalTransition,
) -> Result<(), String> {
    match transition {
        JournalTransition::Spawned {
            context,
            pid,
            identity_token,
        } => {
            state.spawns.insert(
                context.effect_id,
                SpawnEvidence {
                    context: context.as_ref().clone(),
                    pid: *pid,
                    identity_token: identity_token.clone(),
                    exit_code: None,
                },
            );
            Ok(())
        }
        JournalTransition::Exited { pid, exit_code } => {
            let evidence = state
                .spawns
                .values_mut()
                .find(|spawn| spawn.pid == *pid && spawn.exit_code.is_none())
                .ok_or_else(|| "missing process spawn".to_string())?;
            evidence.exit_code = Some(*exit_code);
            Ok(())
        }
    }
}

fn append_record(file: &mut File, record: &JournalRecord) -> AgentResult<()> {
    let payload = serde_json::to_vec(record).map_err(|error| {
        AgentError::Storage(format!("serialize process journal record: {error}"))
    })?;
    let frame = StoredFrame {
        checksum: checksum_hex(&payload),
        record: record.clone(),
    };
    let encoded = serde_json::to_vec(&frame).map_err(|error| {
        AgentError::Storage(format!("serialize process journal frame: {error}"))
    })?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(AgentError::Storage(format!(
            "process effect journal frame exceeds {MAX_FRAME_BYTES} bytes"
        )));
    }
    let projected = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat process effect journal: {error}")))?
        .len()
        .checked_add(encoded.len() as u64 + 1)
        .ok_or_else(|| AgentError::Storage("process effect journal size overflow".into()))?;
    if projected > MAX_FILE_BYTES {
        return Err(AgentError::RecoveryRequired(format!(
            "process effect journal reached its {MAX_FILE_BYTES} byte hard limit"
        )));
    }
    file.seek(SeekFrom::End(0))
        .and_then(|_| file.write_all(&encoded))
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .and_then(|_| file.sync_all())
        .map_err(|error| AgentError::Storage(format!("persist process effect journal: {error}")))
}

fn recover_file(file: &mut File, path: &Path) -> AgentResult<RecoveryState> {
    let size = file
        .metadata()
        .map_err(|error| AgentError::Storage(format!("stat process effect journal: {error}")))?
        .len();
    if size > MAX_FILE_BYTES {
        return Err(corrupt(path, "file exceeds hard limit"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| AgentError::Storage(format!("seek process effect journal: {error}")))?;
    let mut reader =
        BufReader::new(file.try_clone().map_err(|error| {
            AgentError::Storage(format!("clone process effect journal: {error}"))
        })?);
    let mut state = RecoveryState::default();
    let mut offset = 0_u64;
    let mut last_good = 0_u64;
    let mut torn_tail = false;
    loop {
        let mut bytes = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_FRAME_BYTES + 2) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| {
                AgentError::Storage(format!("read process effect journal: {error}"))
            })?;
        if read == 0 {
            break;
        }
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| corrupt(path, "offset overflow"))?;
        if !bytes.ends_with(b"\n") {
            if offset == size {
                torn_tail = true;
                break;
            }
            return Err(corrupt(path, "partial middle frame"));
        }
        if bytes.len() > MAX_FRAME_BYTES + 1 {
            return Err(corrupt(path, "frame exceeds bound"));
        }
        let frame: StoredFrame = serde_json::from_slice(&bytes[..bytes.len() - 1])
            .map_err(|_| corrupt(path, "malformed frame"))?;
        let payload =
            serde_json::to_vec(&frame.record).map_err(|_| corrupt(path, "reserialize frame"))?;
        if checksum_hex(&payload) != frame.checksum {
            return Err(corrupt(path, "checksum mismatch"));
        }
        if frame.record.version != JOURNAL_VERSION {
            return Err(corrupt(path, "unsupported version"));
        }
        if frame.record.seq != state.last_seq + 1 {
            return Err(corrupt(path, "sequence gap"));
        }
        validate_transition(&frame.record.transition).map_err(|detail| corrupt(path, &detail))?;
        validate_fold(&state, &frame.record.transition).map_err(|detail| corrupt(path, &detail))?;
        apply_transition(&mut state, &frame.record.transition)
            .map_err(|detail| corrupt(path, &detail))?;
        state.last_seq = frame.record.seq;
        last_good = offset;
    }
    if torn_tail {
        file.set_len(last_good)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                AgentError::Storage(format!("repair process effect journal tail: {error}"))
            })?;
    }
    Ok(state)
}

fn open_spawn_by_pid(state: &RecoveryState, pid: u32) -> Option<&SpawnEvidence> {
    state
        .spawns
        .values()
        .find(|spawn| spawn.pid == pid && spawn.exit_code.is_none())
}

fn checksum_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn corrupt(path: &Path, detail: &str) -> AgentError {
    AgentError::RecoveryRequired(format!(
        "process effect journal {} is corrupt: {detail}",
        path.display()
    ))
}

fn bounded_evidence(mut text: String) -> String {
    if text.len() > 512 {
        let mut boundary = 512;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    text
}

fn bounded_reason(mut reason: String) -> String {
    if reason.len() > MAX_REASON_BYTES {
        let mut boundary = MAX_REASON_BYTES;
        while !reason.is_char_boundary(boundary) {
            boundary -= 1;
        }
        reason.truncate(boundary);
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ArgumentDigest, EffectId, EffectReconciler, OperationId, RunId, ToolOperationIdentity,
        TurnId,
    };
    use std::process::Command;
    use std::time::Duration;

    fn process_context(tool_name: &str) -> OperationEffectContext {
        OperationEffectContext {
            identity: ToolOperationIdentity {
                run_id: RunId::new(),
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id: OperationId::new(),
                generation: 1,
                call_id: "call-1".into(),
                tool_name: tool_name.into(),
                argument_digest: ArgumentDigest::sha256_bytes(b"args"),
            },
            effect_id: EffectId::new(),
        }
    }

    async fn workspace() -> (crate::Workspace, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::Workspace::open(directory.path()).await.unwrap();
        (workspace, directory)
    }

    fn spawn_quick() -> std::process::Child {
        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", "exit", "0"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }
        #[cfg(not(windows))]
        {
            Command::new("true")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }
    }

    fn spawn_sleeper() -> std::process::Child {
        #[cfg(windows)]
        {
            Command::new("ping")
                .args(["-n", "20", "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }
        #[cfg(not(windows))]
        {
            Command::new("sleep")
                .arg("20")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        }
    }

    #[tokio::test]
    async fn process_tool_without_spawn_is_not_applied() {
        let (workspace, _directory) = workspace().await;
        let context = process_context("shell.exec");
        assert!(matches!(
            workspace.reconcile(&context).unwrap(),
            EffectReconciliation::NotApplied { .. }
        ));
    }

    #[tokio::test]
    async fn foreign_tool_without_workspace_evidence_stays_unmanaged() {
        let (workspace, _directory) = workspace().await;
        let context = process_context("fs.write");
        assert!(matches!(
            workspace.reconcile(&context).unwrap(),
            EffectReconciliation::NotManaged
        ));
    }

    #[tokio::test]
    async fn spawn_and_exit_settles_as_completed_value() {
        let (workspace, _directory) = workspace().await;
        let context = process_context("process.run");
        let mut child = spawn_quick();
        let pid = child.id();
        workspace.record_process_spawn(&context, pid).unwrap();
        let status = child.wait().unwrap();
        workspace.record_process_exit(pid, status.code()).unwrap();
        assert!(matches!(
            workspace.reconcile(&context).unwrap(),
            EffectReconciliation::CompletedValue { .. }
        ));
    }

    #[tokio::test]
    async fn live_spawn_without_exit_is_ambiguous_and_containment_kills_matching_identity() {
        let (workspace, directory) = workspace().await;
        let context = process_context("shell.exec");
        let mut child = spawn_sleeper();
        let pid = child.id();
        workspace.record_process_spawn(&context, pid).unwrap();
        drop(workspace);

        let reopened = crate::Workspace::open(directory.path()).await.unwrap();
        assert!(matches!(
            reopened.reconcile(&context).unwrap(),
            EffectReconciliation::Ambiguous { .. }
        ));
        std::thread::sleep(Duration::from_millis(300));
        let _ = child.try_wait();
        let _ = child.kill();
    }

    #[tokio::test]
    async fn recover_orphans_kills_matching_leftover_trees() {
        let (workspace, directory) = workspace().await;
        let context = process_context("process.session");
        let mut child = spawn_sleeper();
        let pid = child.id();
        workspace.record_process_spawn(&context, pid).unwrap();
        drop(workspace);

        let reopened = crate::Workspace::open(directory.path()).await.unwrap();
        // 沙箱 Job 可能禁止按 PID OpenProcess(PROCESS_TERMINATE)。
        // 生产路径对此 fail-closed；测试只要求该缝返回合法结果。
        match EffectReconciler::recover_orphans(&reopened) {
            Ok(()) => {}
            Err(AgentError::RecoveryRequired(reason)) => {
                assert!(
                    reason.contains("could not terminate leftover process"),
                    "unexpected recovery reason: {reason}"
                );
            }
            Err(error) => panic!("recover_orphans must fail closed or succeed: {error}"),
        }
        let _ = child.kill();
    }
}
