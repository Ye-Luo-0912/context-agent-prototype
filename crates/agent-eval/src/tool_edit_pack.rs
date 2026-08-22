//! Versioned, provider-independent inputs for the focused Tool Edit gate.
//!
//! The pack keeps exact seed and golden bytes in JSON string escapes. This
//! makes CRLF and mixed-EOL fixtures portable while still letting the
//! self-check compare SHA-256 identities instead of normalized text.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA: &str = "agent-eval.tool-edit.v2";
pub const TASK_COUNT: usize = 4;
pub const DEFAULT_REPEATS: u32 = 3;
const UNEXPECTED_FILES_CAP: usize = 64;
const WORKSPACE_SCAN_ENTRY_CAP: usize = 4_096;
pub const TASK_IDS: [&str; TASK_COUNT] = [
    "crlf_multi_hunk",
    "mixed_eol",
    "stale_revision_recovery",
    "batch_two_file",
];

/// Frozen question and scoring intent. Exact task bytes are additionally
/// covered by [`pack_digest`].
pub const SPEC: &str = "\
schema=agent-eval.tool-edit.v2
question=can the production tool surface apply exact revision-aware edits reliably
surface=production ToolLifecycleConfig; edit.patch is the canonical mutation
engine=dynamic fixed; this is not a context-engine comparison
tasks=crlf_multi_hunk,mixed_eol,stale_revision_recovery,batch_two_file
repeats=3
scoring=raw-byte hidden verification plus frozen trace and exact-hunk contracts
fallback=shell.exec,process.run,process.session,fs.write,edit.replace forbidden
stale=safety refusal or proactive revalidation must preserve concurrent content
";

#[derive(Debug, Clone)]
pub struct ToolEditPack {
    pub root: PathBuf,
    pub tasks: Vec<ToolEditTask>,
}

impl ToolEditPack {
    pub fn task(&self, id: &str) -> Option<&ToolEditTask> {
        self.tasks.iter().find(|task| task.id() == id)
    }
}

#[derive(Debug, Clone)]
pub struct ToolEditTask {
    pub file: ToolEditTaskFile,
    pub path: PathBuf,
}

impl ToolEditTask {
    pub fn id(&self) -> &str {
        &self.file.id
    }

    pub fn user_turns(&self) -> impl Iterator<Item = &str> {
        self.file.ops.iter().filter_map(|op| match op {
            ToolEditOp::User { text } => Some(text.as_str()),
            ToolEditOp::FixtureReplace { .. } => None,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEditTaskFile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub expected_edit: String,
    pub target_rounds_lo: u32,
    pub target_rounds_hi: u32,
    pub seed_files: Vec<FixtureFile>,
    pub golden_files: Vec<FixtureFile>,
    pub ops: Vec<ToolEditOp>,
    pub trace: TraceContract,
}

/// An exact UTF-8 fixture body. JSON `\r\n` escapes become the intended raw
/// bytes after deserialization; the declared digest makes accidental EOL
/// conversion fail closed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FixtureFile {
    pub path: String,
    pub content: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToolEditOp {
    User {
        text: String,
    },
    /// Eval-only, model-invisible write between user turns. The runner must
    /// compare `expected_sha256` before writing, so an unexpected prior edit
    /// aborts the cell instead of overwriting it.
    FixtureReplace {
        path: String,
        expected_sha256: String,
        content: String,
    },
}

/// Model-invisible fixture mutation persisted beside a cell. The optional
/// boundary keeps old diagnostic sidecars readable, while the current gate
/// fails closed unless a new run binds the mutation to a completed turn.
#[derive(Debug, Clone, Deserialize, Serialize, Eq, PartialEq)]
pub struct FixtureMutationRecord {
    pub op_index: usize,
    pub path: String,
    pub before_sha256: String,
    pub after_sha256: String,
    pub bytes_after: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_seq_before: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictContract {
    None,
    StaleOrRevalidated,
}

/// Frozen event-trace expectations consumed by the live gate. Content
/// correctness remains independent in [`evaluate_strict`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TraceContract {
    pub target_files: Vec<String>,
    pub required_successful_patch_calls: usize,
    pub max_patch_calls: usize,
    pub expected_files_per_success: usize,
    pub min_hunks_per_success: usize,
    /// The exact local edits permitted in every patch attempt. Comparing
    /// bounded content digests prevents whole-file or sentinel detours from
    /// satisfying the trace contract merely by ending on the right bytes.
    pub exact_hunks: Vec<TraceHunk>,
    pub require_base_revision: bool,
    pub first_patch_must_succeed: bool,
    pub conflict: ConflictContract,
    pub forbidden_tools: Vec<String>,
    pub max_confirm_reads_after_success: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, PartialEq)]
pub struct TraceHunk {
    pub path: String,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct StrictFileResult {
    pub path: String,
    pub expected_sha256: String,
    pub actual_sha256: Option<String>,
    pub bytes: Option<usize>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct StrictReport {
    pub schema: String,
    pub fixture_id: String,
    pub passed: bool,
    pub files: Vec<StrictFileResult>,
    pub unexpected_files: Vec<String>,
    pub unexpected_files_truncated: bool,
}

#[derive(Debug, Deserialize)]
struct PackFile {
    schema: String,
    tasks: Vec<String>,
}

pub fn pack_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tool-edit")
}

pub fn load_pack() -> anyhow::Result<ToolEditPack> {
    load_pack_from(&pack_root())
}

pub fn load_pack_from(root: &Path) -> anyhow::Result<ToolEditPack> {
    let manifest_path = root.join("pack.json");
    let manifest: PackFile = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", manifest_path.display()))?,
    )?;
    if manifest.schema != SCHEMA {
        anyhow::bail!("tool-edit pack schema {} != {SCHEMA}", manifest.schema);
    }
    let mut tasks = Vec::with_capacity(manifest.tasks.len());
    for relative in manifest.tasks {
        validate_relative(&relative)?;
        let path = root.join(&relative);
        let file: ToolEditTaskFile = serde_json::from_str(
            &fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?,
        )?;
        tasks.push(ToolEditTask { file, path });
    }
    let pack = ToolEditPack {
        root: root.to_path_buf(),
        tasks,
    };
    validate_pack_shape(&pack)?;
    Ok(pack)
}

/// Materialize the exact raw seed into a fresh workspace.
pub fn seed_task(task: &ToolEditTask, root: &Path) -> anyhow::Result<()> {
    materialize_files(&task.file.seed_files, root)
}

/// Materialize the exact accepted result. Used only by pack self-checks.
pub fn apply_golden(task: &ToolEditTask, root: &Path) -> anyhow::Result<()> {
    materialize_files(&task.file.golden_files, root)
}

/// Compare raw file SHA-256 values and reject unexpected model-created files.
/// Runtime-owned `.git` / `.focus-agent` trees and the exact harness-created
/// `.gitignore` are intentionally excluded.
pub fn evaluate_strict(task: &ToolEditTask, root: &Path) -> anyhow::Result<StrictReport> {
    let expected_paths: BTreeSet<String> = task
        .file
        .golden_files
        .iter()
        .map(|file| normalize_relative(&file.path))
        .collect();
    let mut files = Vec::with_capacity(task.file.golden_files.len());
    for expected in &task.file.golden_files {
        validate_relative(&expected.path)?;
        let path = root.join(&expected.path);
        let (actual_sha256, bytes) = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let bytes = fs::read(&path)?;
                (Some(content_sha256(&bytes)), Some(bytes.len()))
            }
            Ok(_) => (None, None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
            Err(error) => return Err(error.into()),
        };
        let passed = actual_sha256.as_deref() == Some(expected.sha256.as_str());
        files.push(StrictFileResult {
            path: normalize_relative(&expected.path),
            expected_sha256: expected.sha256.clone(),
            actual_sha256,
            bytes,
            passed,
        });
    }
    let actual = collect_workspace_files(
        root,
        expected_paths
            .len()
            .saturating_add(UNEXPECTED_FILES_CAP)
            .saturating_add(1),
    )?;
    let mut unexpected_files: Vec<String> =
        actual.paths.difference(&expected_paths).cloned().collect();
    let unexpected_files_truncated =
        actual.truncated || unexpected_files.len() > UNEXPECTED_FILES_CAP;
    unexpected_files.truncate(UNEXPECTED_FILES_CAP);
    let passed = files.iter().all(|file| file.passed)
        && unexpected_files.is_empty()
        && !unexpected_files_truncated;
    Ok(StrictReport {
        schema: SCHEMA.to_string(),
        fixture_id: task.id().to_string(),
        passed,
        files,
        unexpected_files,
        unexpected_files_truncated,
    })
}

/// Validate identities and prove that every seed fails while every golden
/// workspace passes the exact hidden check.
pub fn check_pack(pack: &ToolEditPack) -> anyhow::Result<String> {
    validate_pack_shape(pack)?;
    let mut out = String::new();
    for task in &pack.tasks {
        validate_task(task)?;

        let seed = tempfile::tempdir()?;
        seed_task(task, seed.path())?;
        if evaluate_strict(task, seed.path())?.passed {
            anyhow::bail!("{} seed already passes strict verification", task.id());
        }

        let golden = tempfile::tempdir()?;
        apply_golden(task, golden.path())?;
        let report = evaluate_strict(task, golden.path())?;
        if !report.passed {
            anyhow::bail!(
                "{} golden failed strict verification: {report:?}",
                task.id()
            );
        }
        out.push_str(&format!("ok {} {}\n", task.id(), task_sha256(task)?));
    }
    Ok(out)
}

pub fn render_pack(pack: &ToolEditPack) -> String {
    let mut out = format!(
        "schema={SCHEMA}\nspec_sha256={}\npack_digest={}\ntasks={} repeats={} cells={}\n",
        spec_sha256(),
        pack_digest(pack).unwrap_or_else(|error| format!("error:{error}")),
        pack.tasks.len(),
        DEFAULT_REPEATS,
        pack.tasks.len() as u32 * DEFAULT_REPEATS,
    );
    for task in &pack.tasks {
        out.push_str(&format!(
            "  {:<28} files={} rounds={}..{}  {}\n",
            task.id(),
            task.file.golden_files.len(),
            task.file.target_rounds_lo,
            task.file.target_rounds_hi,
            task.file.name,
        ));
    }
    out
}

pub fn spec_sha256() -> String {
    content_sha256(SPEC.as_bytes())
}

pub fn task_sha256(task: &ToolEditTask) -> anyhow::Result<String> {
    Ok(content_sha256(&fs::read(&task.path)?))
}

pub fn pack_digest(pack: &ToolEditPack) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(SCHEMA.as_bytes());
    hasher.update([0]);
    hasher.update(SPEC.as_bytes());
    hasher.update([0]);
    hasher.update(fs::read(pack.root.join("pack.json"))?);
    hasher.update([0]);
    for task in &pack.tasks {
        hasher.update(task.id().as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(&task.path)?);
        hasher.update([0]);
    }
    Ok(hex_encode(hasher.finalize()))
}

pub fn content_sha256(bytes: &[u8]) -> String {
    hex_encode(Sha256::digest(bytes))
}

fn validate_pack_shape(pack: &ToolEditPack) -> anyhow::Result<()> {
    if pack.tasks.len() != TASK_COUNT {
        anyhow::bail!(
            "tool-edit pack has {} tasks, expected {TASK_COUNT}",
            pack.tasks.len()
        );
    }
    let ids: BTreeSet<&str> = pack.tasks.iter().map(|task| task.id()).collect();
    if ids.len() != TASK_COUNT {
        anyhow::bail!("tool-edit pack contains duplicate task ids");
    }
    for expected in TASK_IDS {
        if !ids.contains(expected) {
            anyhow::bail!("tool-edit pack is missing {expected}");
        }
    }
    Ok(())
}

fn validate_task(task: &ToolEditTask) -> anyhow::Result<()> {
    if task.file.name.trim().is_empty()
        || task.file.description.trim().is_empty()
        || task.file.expected_edit.trim().is_empty()
    {
        anyhow::bail!("{} has an empty required description field", task.id());
    }
    if task.file.target_rounds_lo == 0 || task.file.target_rounds_hi < task.file.target_rounds_lo {
        anyhow::bail!("{} has an invalid target round range", task.id());
    }
    if task.user_turns().next().is_none() {
        anyhow::bail!("{} has no model-visible user turn", task.id());
    }
    validate_fixture_files(task.id(), "seed", &task.file.seed_files)?;
    validate_fixture_files(task.id(), "golden", &task.file.golden_files)?;

    let seeds: BTreeSet<String> = task
        .file
        .seed_files
        .iter()
        .map(|file| normalize_relative(&file.path))
        .collect();
    let golden: BTreeSet<String> = task
        .file
        .golden_files
        .iter()
        .map(|file| normalize_relative(&file.path))
        .collect();
    if seeds != golden {
        anyhow::bail!("{} seed/golden path sets differ", task.id());
    }
    let targets: BTreeSet<String> = task
        .file
        .trace
        .target_files
        .iter()
        .map(|path| normalize_relative(path))
        .collect();
    if targets != golden {
        anyhow::bail!("{} trace targets differ from golden paths", task.id());
    }
    if task.file.trace.required_successful_patch_calls == 0
        || task.file.trace.max_patch_calls < task.file.trace.required_successful_patch_calls
        || task.file.trace.expected_files_per_success == 0
        || task.file.trace.min_hunks_per_success == 0
    {
        anyhow::bail!("{} has an invalid trace cardinality", task.id());
    }
    if task.file.trace.exact_hunks.len() < task.file.trace.min_hunks_per_success
        || task.file.trace.exact_hunks.len() > 64
    {
        anyhow::bail!("{} has an invalid exact-hunk cardinality", task.id());
    }
    let mut exact_hunks = BTreeSet::new();
    let mut exact_hunk_bytes = 0usize;
    for hunk in &task.file.trace.exact_hunks {
        validate_relative(&hunk.path)?;
        let path = normalize_relative(&hunk.path);
        if !targets.contains(&path) || hunk.old.is_empty() || hunk.old == hunk.new {
            anyhow::bail!("{} has an invalid exact hunk for {}", task.id(), hunk.path);
        }
        if hunk.old.len() > 64 * 1024 || hunk.new.len() > 64 * 1024 {
            anyhow::bail!("{} has an oversized exact hunk", task.id());
        }
        exact_hunk_bytes = exact_hunk_bytes
            .checked_add(hunk.old.len())
            .and_then(|total| total.checked_add(hunk.new.len()))
            .ok_or_else(|| anyhow::anyhow!("{} exact-hunk byte count overflowed", task.id()))?;
        if exact_hunk_bytes > 256 * 1024 {
            anyhow::bail!("{} exact hunks exceed the byte budget", task.id());
        }
        if !exact_hunks.insert((path, hunk.old.as_str(), hunk.new.as_str())) {
            anyhow::bail!("{} has a duplicate exact hunk", task.id());
        }
    }

    let mut mutation_count = 0usize;
    for op in &task.file.ops {
        if let ToolEditOp::FixtureReplace {
            path,
            expected_sha256,
            content,
        } = op
        {
            mutation_count += 1;
            validate_relative(path)?;
            validate_sha256(expected_sha256)?;
            let seeded = task
                .file
                .seed_files
                .iter()
                .find(|file| normalize_relative(&file.path) == normalize_relative(path))
                .ok_or_else(|| {
                    anyhow::anyhow!("{} fixture replace path is not seeded", task.id())
                })?;
            if seeded.sha256.as_str() != expected_sha256.as_str() {
                anyhow::bail!(
                    "{} fixture replace precondition does not match seed",
                    task.id()
                );
            }
            if content_sha256(content.as_bytes()) == expected_sha256.as_str() {
                anyhow::bail!("{} fixture replace is a no-op", task.id());
            }
        }
    }
    match task.file.trace.conflict {
        ConflictContract::None if mutation_count != 0 => {
            anyhow::bail!(
                "{} non-conflict task contains a fixture mutation",
                task.id()
            )
        }
        ConflictContract::StaleOrRevalidated if mutation_count != 1 => anyhow::bail!(
            "{} stale task must contain exactly one fixture mutation",
            task.id()
        ),
        _ => {}
    }
    Ok(())
}

fn validate_fixture_files(id: &str, kind: &str, files: &[FixtureFile]) -> anyhow::Result<()> {
    if files.is_empty() {
        anyhow::bail!("{id} has no {kind} files");
    }
    let mut paths = BTreeSet::new();
    for file in files {
        validate_relative(&file.path)?;
        validate_sha256(&file.sha256)?;
        if !paths.insert(normalize_relative(&file.path)) {
            anyhow::bail!("{id} has duplicate {kind} path {}", file.path);
        }
        let actual = content_sha256(file.content.as_bytes());
        if actual != file.sha256 {
            anyhow::bail!(
                "{id} {kind} {} sha256 {} != declared {}",
                file.path,
                actual,
                file.sha256
            );
        }
    }
    Ok(())
}

fn validate_sha256(value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid SHA-256 hex {value:?}");
    }
    Ok(())
}

fn validate_relative(value: &str) -> anyhow::Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        anyhow::bail!("fixture path must be a clean relative path: {value:?}");
    }
    Ok(())
}

fn normalize_relative(path: &str) -> String {
    path.replace('\\', "/")
}

fn materialize_files(files: &[FixtureFile], root: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(root)?;
    for file in files {
        validate_relative(&file.path)?;
        let target = root.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, file.content.as_bytes())?;
    }
    Ok(())
}

struct WorkspaceScan {
    paths: BTreeSet<String>,
    entries_seen: usize,
    truncated: bool,
}

fn collect_workspace_files(root: &Path, limit: usize) -> anyhow::Result<WorkspaceScan> {
    let mut scan = WorkspaceScan {
        paths: BTreeSet::new(),
        entries_seen: 0,
        truncated: false,
    };
    collect_workspace_files_inner(root, root, limit, &mut scan)?;
    Ok(scan)
}

fn collect_workspace_files_inner(
    root: &Path,
    dir: &Path,
    limit: usize,
    scan: &mut WorkspaceScan,
) -> anyhow::Result<()> {
    if !dir.is_dir() || scan.truncated {
        return Ok(());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        if scan.entries_seen >= WORKSPACE_SCAN_ENTRY_CAP {
            scan.truncated = true;
            break;
        }
        scan.entries_seen += 1;
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if scan.paths.len() >= limit {
            scan.truncated = true;
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if dir == root && (name == ".git" || name == ".focus-agent") {
                continue;
            }
            collect_workspace_files_inner(root, &path, limit, scan)?;
        } else {
            let relative = path.strip_prefix(root)?;
            let relative = normalize_relative(&relative.to_string_lossy());
            if file_type.is_file()
                && relative == ".gitignore"
                && fs::read(&path)? == b".focus-agent/\n"
            {
                continue;
            }
            scan.paths.insert(relative);
        }
    }
    Ok(())
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_self_check_is_green() {
        let pack = load_pack().expect("tool-edit pack");
        let report = check_pack(&pack).expect("tool-edit self-check");
        assert_eq!(report.lines().count(), TASK_COUNT);
        assert_eq!(pack_digest(&pack).unwrap().len(), 64);
        assert_eq!(spec_sha256().len(), 64);
    }

    #[test]
    fn raw_line_endings_survive_json_and_materialization() {
        let pack = load_pack().unwrap();
        let crlf = pack.task("crlf_multi_hunk").unwrap();
        let crlf_bytes = crlf.file.seed_files[0].content.as_bytes();
        assert!(crlf_bytes.windows(2).any(|pair| pair == b"\r\n"));
        assert!(
            !crlf_bytes
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte == b'\n'
                    && (index == 0 || crlf_bytes[index - 1] != b'\r'))
        );

        let mixed = pack.task("mixed_eol").unwrap();
        let mixed_bytes = mixed.file.seed_files[0].content.as_bytes();
        assert!(mixed_bytes.windows(2).any(|pair| pair == b"\r\n"));
        assert!(
            mixed_bytes
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte == b'\n'
                    && (index == 0 || mixed_bytes[index - 1] != b'\r'))
        );
    }

    #[test]
    fn every_seed_fails_and_every_golden_passes_strict_sha() {
        let pack = load_pack().unwrap();
        for task in &pack.tasks {
            let seed = tempfile::tempdir().unwrap();
            seed_task(task, seed.path()).unwrap();
            assert!(
                !evaluate_strict(task, seed.path()).unwrap().passed,
                "{}",
                task.id()
            );

            let golden = tempfile::tempdir().unwrap();
            apply_golden(task, golden.path()).unwrap();
            assert!(
                evaluate_strict(task, golden.path()).unwrap().passed,
                "{}",
                task.id()
            );
            fs::write(golden.path().join("unexpected.txt"), b"noise").unwrap();
            let report = evaluate_strict(task, golden.path()).unwrap();
            assert!(!report.passed);
            assert_eq!(report.unexpected_files, vec!["unexpected.txt"]);
        }
    }

    #[test]
    fn unexpected_file_report_is_bounded_and_fails_closed() {
        let pack = load_pack().unwrap();
        let task = pack.task("crlf_multi_hunk").unwrap();
        let golden = tempfile::tempdir().unwrap();
        apply_golden(task, golden.path()).unwrap();
        for index in 0..=UNEXPECTED_FILES_CAP {
            fs::write(
                golden.path().join(format!("unexpected-{index:03}.txt")),
                b"noise",
            )
            .unwrap();
        }

        let report = evaluate_strict(task, golden.path()).unwrap();
        assert!(!report.passed);
        assert_eq!(report.unexpected_files.len(), UNEXPECTED_FILES_CAP);
        assert!(report.unexpected_files_truncated);
    }

    #[test]
    fn strict_check_rejects_non_files_and_nested_runtime_named_directories() {
        let pack = load_pack().unwrap();
        let task = pack.task("crlf_multi_hunk").unwrap();

        let non_file = tempfile::tempdir().unwrap();
        apply_golden(task, non_file.path()).unwrap();
        let expected = non_file.path().join("src/settings.cfg");
        fs::remove_file(&expected).unwrap();
        fs::create_dir(&expected).unwrap();
        assert!(!evaluate_strict(task, non_file.path()).unwrap().passed);

        let nested = tempfile::tempdir().unwrap();
        apply_golden(task, nested.path()).unwrap();
        let nested_git = nested.path().join("src/.git");
        fs::create_dir(&nested_git).unwrap();
        fs::write(nested_git.join("unexpected.txt"), b"noise").unwrap();
        let report = evaluate_strict(task, nested.path()).unwrap();
        assert!(!report.passed);
        assert_eq!(report.unexpected_files, vec!["src/.git/unexpected.txt"]);
    }

    #[test]
    fn stale_fixture_has_a_fail_closed_external_replace() {
        let pack = load_pack().unwrap();
        let task = pack.task("stale_revision_recovery").unwrap();
        let replacement = task.file.ops.iter().find_map(|op| match op {
            ToolEditOp::FixtureReplace {
                path,
                expected_sha256,
                content,
            } => Some((path, expected_sha256, content)),
            ToolEditOp::User { .. } => None,
        });
        let (path, before, content) = replacement.expect("fixture replacement");
        assert_eq!(path, "src/retry.cfg");
        assert_eq!(before, &task.file.seed_files[0].sha256);
        assert_ne!(content_sha256(content.as_bytes()), before.as_str());
        assert_eq!(
            task.file.trace.conflict,
            ConflictContract::StaleOrRevalidated
        );
    }
}
