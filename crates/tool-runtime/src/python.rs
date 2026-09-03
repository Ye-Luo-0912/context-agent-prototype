//! Deterministic Python interpreter discovery for host-owned verification.
//!
//! A command name existing on `PATH` is not enough on Windows: the Store
//! aliases successfully resolve to an executable but exit with 9009 instead
//! of running Python. Discovery therefore executes a bounded semantic probe
//! and retains only the absolute interpreter path reported by Python itself.
//! Symlink spelling is preserved because a virtual environment's launcher is
//! part of its package/import semantics. Callers never receive an unresolved
//! fallback name.

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Product-wide explicit Python interpreter override. The value is one
/// executable path, not a shell command or an argument string.
pub const PYTHON_EXECUTABLE_ENV: &str = "AGENT_PYTHON";

const PROBE_MARKER: &str = "agent-python-v1:";
const MAX_PROBE_OUTPUT_BYTES: usize = 4 * 1024;
const PROBE_CAPTURE_BYTES: usize = MAX_PROBE_OUTPUT_BYTES + 1;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROBE_READER_REAP_GRACE: Duration = Duration::from_secs(1);

/// A Python 3 interpreter that completed the semantic probe. The executable
/// is absolute and UTF-8 because verification recipes cross a string-argv
/// contract. It is deliberately not canonicalized through symlinks: doing so
/// would escape a virtual environment back to its base interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonInterpreter {
    executable: String,
}

impl PythonInterpreter {
    pub(crate) fn from_probed_executable(executable: String) -> Result<Self, String> {
        if !Path::new(&executable).is_absolute() {
            return Err("probe returned a non-absolute executable identity".into());
        }
        if !std::fs::metadata(&executable)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Err("reported executable identity is not a file".into());
        }
        Ok(Self { executable })
    }

    pub fn executable(&self) -> &str {
        &self.executable
    }

    /// Build argv without a launcher shim. `py -3` is a discovery candidate,
    /// but successful discovery records and executes the actual interpreter.
    pub fn command_argv<I, S>(&self, args: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        std::iter::once(self.executable.clone())
            .chain(args.into_iter().map(Into::into))
            .collect()
    }
}

/// Typed setup failure. Explicit configuration is authoritative and never
/// silently falls through to another interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonInterpreterError {
    ExplicitInvalid {
        variable: String,
        configured: String,
        reason: String,
    },
    Unavailable {
        attempts: Vec<String>,
    },
}

impl fmt::Display for PythonInterpreterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExplicitInvalid {
                variable,
                configured,
                reason,
            } => write!(
                formatter,
                "python_interpreter_invalid: {variable}={configured:?} did not identify a working Python 3 interpreter ({reason})"
            ),
            Self::Unavailable { attempts } => write!(
                formatter,
                "python_interpreter_unavailable: no working Python 3 interpreter found ({})",
                attempts.join("; ")
            ),
        }
    }
}

impl std::error::Error for PythonInterpreterError {}

#[derive(Debug, Clone)]
struct PythonCandidate {
    label: String,
    program: OsString,
    launcher_args: Vec<OsString>,
}

impl PythonCandidate {
    fn explicit(variable: &str, configured: OsString) -> Self {
        Self {
            label: format!("{variable}={:?}", configured.to_string_lossy()),
            program: configured,
            launcher_args: Vec::new(),
        }
    }

    fn named(program: &str, launcher_args: &[&str]) -> Self {
        Self {
            label: std::iter::once(program)
                .chain(launcher_args.iter().copied())
                .collect::<Vec<_>>()
                .join(" "),
            program: OsString::from(program),
            launcher_args: launcher_args.iter().map(OsString::from).collect(),
        }
    }
}

/// Resolve Python from the first explicitly configured environment variable,
/// then `py -3`, `python3`, and `python`. Every candidate must execute the
/// Python 3 probe; path lookup alone is never accepted.
pub fn resolve_python_interpreter(
    explicit_env_vars: &[&str],
) -> Result<PythonInterpreter, PythonInterpreterError> {
    let explicit = explicit_env_vars.iter().find_map(|variable| {
        std::env::var_os(variable).map(|configured| ((*variable).to_string(), configured))
    });
    resolve_with(explicit, probe_candidate)
}

/// Resolve one already-loaded explicit override. Hosts that keep local
/// configuration outside the process environment can use the same semantic
/// probe without mutating global environment state.
pub fn resolve_python_interpreter_value(
    variable: &str,
    configured: OsString,
) -> Result<PythonInterpreter, PythonInterpreterError> {
    resolve_with(Some((variable.to_string(), configured)), probe_candidate)
}

fn resolve_with<F>(
    explicit: Option<(String, OsString)>,
    mut probe: F,
) -> Result<PythonInterpreter, PythonInterpreterError>
where
    F: FnMut(&PythonCandidate) -> Result<String, String>,
{
    if let Some((variable, configured)) = explicit {
        if configured.is_empty() {
            return Err(PythonInterpreterError::ExplicitInvalid {
                variable,
                configured: String::new(),
                reason: "configured value is empty".into(),
            });
        }
        let candidate = PythonCandidate::explicit(&variable, configured.clone());
        return probe(&candidate)
            .and_then(PythonInterpreter::from_probed_executable)
            .map_err(|reason| PythonInterpreterError::ExplicitInvalid {
                variable,
                configured: configured.to_string_lossy().into_owned(),
                reason,
            });
    }

    let candidates = [
        PythonCandidate::named("py", &["-3"]),
        PythonCandidate::named("python3", &[]),
        PythonCandidate::named("python", &[]),
    ];
    let mut attempts = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        match probe(&candidate).and_then(PythonInterpreter::from_probed_executable) {
            Ok(interpreter) => return Ok(interpreter),
            Err(reason) => attempts.push(format!("{}: {reason}", candidate.label)),
        }
    }
    Err(PythonInterpreterError::Unavailable { attempts })
}

fn probe_candidate(candidate: &PythonCandidate) -> Result<String, String> {
    // Isolated mode ignores user site packages and Python environment knobs.
    // The script emits exactly one tagged path; anything else is rejected.
    let script = format!(
        "import os,sys; sys.exit(3) if sys.version_info[0] != 3 else print({PROBE_MARKER:?} + os.path.abspath(sys.executable))"
    );
    let mut command = Command::new(&candidate.program);
    command
        .args(&candidate.launcher_args)
        .args(["-I", "-c", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = run_bounded_command(&mut command, PROBE_TIMEOUT)
        .map_err(|error| format!("probe {error}"))?;
    if !output.status.success() {
        return Err(format!("probe exited {}", output.status));
    }
    if output.stdout.overflowed || output.stdout.bytes.len() > MAX_PROBE_OUTPUT_BYTES {
        return Err(format!(
            "probe output exceeded {MAX_PROBE_OUTPUT_BYTES} bytes"
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout.bytes)
        .map_err(|_| "probe output was not UTF-8".to_string())?;
    let reported = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(PROBE_MARKER))
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| "probe did not report its executable identity".to_string())?;
    Ok(reported.to_string())
}

#[derive(Debug)]
struct BoundedCommandOutput {
    status: ExitStatus,
    stdout: BoundedCapture,
}

#[derive(Debug, Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    overflowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BoundedCommandError {
    Spawn(String),
    MissingStdout,
    Wait(String),
    ReaderPanicked,
    TimedOut,
}

impl fmt::Display for BoundedCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(formatter, "could not start: {error}"),
            Self::MissingStdout => formatter.write_str("did not expose stdout"),
            Self::Wait(error) => write!(formatter, "wait failed: {error}"),
            Self::ReaderPanicked => formatter.write_str("stdout reader panicked"),
            Self::TimedOut => formatter.write_str("timed out"),
        }
    }
}

/// Execute one already-constructed command with bounded time and resident
/// output. The reader keeps draining after its small capture fills so an
/// overproducing child cannot block on a full pipe; timeout and wait errors
/// always kill and reap the child before returning.
fn run_bounded_command(
    command: &mut Command,
    timeout: Duration,
) -> Result<BoundedCommandOutput, BoundedCommandError> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| BoundedCommandError::Spawn(error.to_string()))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_and_reap(&mut child);
            return Err(BoundedCommandError::MissingStdout);
        }
    };
    let (reader_tx, reader_rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let _ = reader_tx.send(read_bounded(stdout, PROBE_CAPTURE_BYTES));
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                kill_and_reap(&mut child);
                settle_terminated_reader(reader_rx, reader);
                return Err(BoundedCommandError::TimedOut);
            }
            Ok(None) => std::thread::sleep(PROBE_POLL_INTERVAL.min(timeout)),
            Err(error) => {
                kill_and_reap(&mut child);
                settle_terminated_reader(reader_rx, reader);
                return Err(BoundedCommandError::Wait(error.to_string()));
            }
        }
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    let stdout = match reader_rx.recv_timeout(remaining) {
        Ok(stdout) => {
            reader
                .join()
                .map_err(|_| BoundedCommandError::ReaderPanicked)?;
            stdout
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = reader.join();
            return Err(BoundedCommandError::ReaderPanicked);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // The direct child exited but a descendant inherited stdout.
            // Its PID is no longer owned after try_wait reaped it, so never
            // feed that stale number to a tree-kill operation. Without a
            // retained OS containment handle the safe fallback is to report
            // the timeout and detach the bounded reader, not risk killing an
            // unrelated process after PID reuse.
            settle_terminated_reader(reader_rx, reader);
            return Err(BoundedCommandError::TimedOut);
        }
    };
    Ok(BoundedCommandOutput { status, stdout })
}

fn settle_terminated_reader(
    reader_rx: std::sync::mpsc::Receiver<BoundedCapture>,
    reader: std::thread::JoinHandle<()>,
) {
    match reader_rx.recv_timeout(PROBE_READER_REAP_GRACE) {
        Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = reader.join();
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // Detach rather than turn a bounded probe failure into an
            // unbounded join. The killed tree normally closes the pipe; this
            // fallback also covers hostile or platform-broken inheritance.
            drop(reader);
        }
    }
}

fn kill_and_reap(child: &mut std::process::Child) {
    agent_process::kill_process_tree(child.id());
    // Direct kill is the last-resort complement when a platform tree-kill
    // facility is unavailable or races process exit.
    let _ = child.kill();
    let _ = child.wait();
}

fn read_bounded(mut source: impl Read, cap: usize) -> BoundedCapture {
    let mut capture = BoundedCapture {
        bytes: Vec::with_capacity(cap),
        overflowed: false,
    };
    let mut buffer = [0u8; 1024];
    loop {
        let read = match source.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let remaining = cap.saturating_sub(capture.bytes.len());
        let keep = remaining.min(read);
        capture.bytes.extend_from_slice(&buffer[..keep]);
        if keep < read {
            capture.overflowed = true;
        }
    }
    capture
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_ENV: &str = "TOOL_RUNTIME_PYTHON_PROBE_FIXTURE";
    const FIXTURE_PID_FILE_ENV: &str = "TOOL_RUNTIME_PYTHON_PROBE_PID_FILE";

    fn absolute_test_program() -> String {
        std::fs::canonicalize(std::env::current_exe().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn fallback_order_probes_launcher_then_named_interpreters() {
        let mut seen = Vec::new();
        let interpreter = resolve_with(None, |candidate| {
            seen.push(candidate.label.clone());
            if candidate.label == "python3" {
                Ok(absolute_test_program())
            } else {
                Err("not usable".into())
            }
        })
        .unwrap();
        assert_eq!(seen, ["py -3", "python3"]);
        assert!(Path::new(interpreter.executable()).is_absolute());
    }

    #[test]
    fn invalid_explicit_configuration_never_falls_back() {
        let mut seen = Vec::new();
        let error = resolve_with(
            Some(("TEST_PYTHON".into(), "missing".into())),
            |candidate| {
                seen.push(candidate.label.clone());
                Err("probe failed".into())
            },
        )
        .unwrap_err();
        assert_eq!(seen.len(), 1);
        assert!(matches!(
            error,
            PythonInterpreterError::ExplicitInvalid { variable, .. } if variable == "TEST_PYTHON"
        ));
    }

    #[test]
    fn unavailable_is_typed_and_never_returns_a_placeholder() {
        let error = resolve_with(None, |candidate| {
            Err(format!("{} unavailable", candidate.label))
        })
        .unwrap_err();
        match error {
            PythonInterpreterError::Unavailable { attempts } => {
                assert_eq!(attempts.len(), 3);
                assert!(attempts[0].starts_with("py -3:"));
                assert!(attempts[1].starts_with("python3:"));
                assert!(attempts[2].starts_with("python:"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn command_argv_uses_the_absolute_interpreter_without_launcher_args() {
        let executable = absolute_test_program();
        let interpreter = PythonInterpreter::from_probed_executable(executable.clone()).unwrap();
        assert_eq!(
            interpreter.command_argv(["-m", "pytest"]),
            [executable, "-m".into(), "pytest".into()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_preserves_a_virtual_environment_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let venv_python = dir.path().join("python");
        symlink(std::env::current_exe().unwrap(), &venv_python).unwrap();
        let reported = venv_python.to_string_lossy().into_owned();
        let interpreter = PythonInterpreter::from_probed_executable(reported.clone()).unwrap();
        assert_eq!(interpreter.executable(), reported);
    }

    fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "python::tests::bounded_probe_child_fixture",
                "--nocapture",
            ])
            .env(FIXTURE_ENV, mode)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn bounded_runner_caps_resident_stdout_while_draining_the_child() {
        let output =
            run_bounded_command(&mut fixture_command("overflow"), Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.bytes.len(), PROBE_CAPTURE_BYTES);
        assert!(output.stdout.overflowed);
    }

    #[test]
    fn bounded_runner_kills_and_reaps_a_timed_out_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("probe.pid");
        let mut command = fixture_command("timeout");
        command.env(FIXTURE_PID_FILE_ENV, &pid_file);
        let started = Instant::now();
        let error = run_bounded_command(&mut command, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error, BoundedCommandError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(pid_file)
            .expect("fixture started and recorded its pid")
            .trim()
            .parse()
            .unwrap();
        assert!(!agent_process::process_is_running(pid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_runner_never_joins_forever_on_inherited_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant.pid");
        let mut command = fixture_command("inherited-stdout");
        command.env(FIXTURE_PID_FILE_ENV, &pid_file);
        let started = Instant::now();
        let error = run_bounded_command(&mut command, Duration::from_secs(1)).unwrap_err();
        assert_eq!(error, BoundedCommandError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: u32 = std::fs::read_to_string(pid_file)
            .expect("descendant recorded its pid")
            .trim()
            .parse()
            .unwrap();
        let identity = agent_process::capture_process_identity(pid).unwrap();
        assert!(agent_process::kill_matching_process_tree(
            identity.pid,
            &identity.identity_token
        ));
    }

    #[test]
    #[ignore = "spawned only by bounded probe runner regressions"]
    fn bounded_probe_child_fixture() {
        match std::env::var(FIXTURE_ENV).as_deref() {
            Ok("overflow") => {
                use std::io::Write;
                std::io::stdout()
                    .write_all(&vec![b'x'; PROBE_CAPTURE_BYTES + 1024])
                    .unwrap();
            }
            Ok("timeout") => {
                let pid_file = std::env::var_os(FIXTURE_PID_FILE_ENV)
                    .expect("timeout fixture receives a pid file");
                std::fs::write(pid_file, std::process::id().to_string()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            #[cfg(target_os = "linux")]
            Ok("inherited-stdout") => {
                let pid_file = std::env::var_os(FIXTURE_PID_FILE_ENV)
                    .expect("descendant fixture receives a pid file");
                let mut descendant = std::process::Command::new(std::env::current_exe().unwrap());
                descendant
                    .args([
                        "--ignored",
                        "--exact",
                        "python::tests::bounded_probe_child_fixture",
                        "--nocapture",
                    ])
                    .env(FIXTURE_ENV, "inherited-stdout-descendant")
                    .env(FIXTURE_PID_FILE_ENV, pid_file)
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::null());
                // Give the fixture its own group so identity-checked test
                // cleanup can terminate it without using the already-reaped
                // parent's stale PID. The fixture process must outlive this
                // handler, so the child is reaped on a detached thread.
                use std::os::unix::process::CommandExt;
                descendant.process_group(0);
                let child = descendant.spawn().unwrap();
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            #[cfg(target_os = "linux")]
            Ok("inherited-stdout-descendant") => {
                let pid_file = std::env::var_os(FIXTURE_PID_FILE_ENV)
                    .expect("descendant fixture receives a pid file");
                std::fs::write(pid_file, std::process::id().to_string()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            }
            other => panic!("unexpected probe fixture mode: {other:?}"),
        }
    }
}
