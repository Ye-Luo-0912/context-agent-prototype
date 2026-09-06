//! Product command-line parsing shared by the interactive TUI and the
//! headless JSONL entry. Invalid flags fail before the workspace is opened.

use std::path::PathBuf;
use std::time::Duration;

/// Default wait for one headless turn when `--timeout-secs` is omitted.
pub const DEFAULT_HEADLESS_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_HEADLESS_TIMEOUT_SECS: u64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductArgs {
    pub read_only: bool,
    pub context_policy: String,
    pub root: Option<PathBuf>,
    pub grant_args: Vec<String>,
    pub effect_reservation_journal: Option<PathBuf>,
    pub restore_arg: Option<PathBuf>,
    pub doctor_mode: bool,
    pub max_rounds: Option<usize>,
    pub defer_proof: bool,
    pub prompt: Option<String>,
    pub work: bool,
    pub continue_task: bool,
    pub timeout_secs: Option<u64>,
    pub help: bool,
}

impl ProductArgs {
    pub fn is_headless(&self) -> bool {
        self.prompt.is_some() || self.continue_task
    }

    pub fn headless_timeout(&self) -> Duration {
        self.timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_HEADLESS_TIMEOUT)
    }
}

pub fn parse_args<I, S>(args: I) -> anyhow::Result<ProductArgs>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut parsed = ProductArgs {
        read_only: false,
        context_policy: "dynamic".to_string(),
        root: None,
        grant_args: Vec::new(),
        effect_reservation_journal: None,
        restore_arg: None,
        doctor_mode: false,
        max_rounds: None,
        defer_proof: false,
        prompt: None,
        work: false,
        continue_task: false,
        timeout_secs: None,
        help: false,
    };
    for arg in args {
        let arg = arg.as_ref();
        if arg == "--help" || arg == "-h" {
            parsed.help = true;
        } else if arg == "--yes" || arg == "--allow-all" || arg == "-y" {
            anyhow::bail!(
                "{arg} is not a product flag: a headless session never implies allow-all. \
                 Pass --grant=<JSON> for unattended writes, or --read-only"
            );
        } else if arg == "--doctor" {
            parsed.doctor_mode = true;
        } else if arg == "--read-only" {
            parsed.read_only = true;
        } else if arg == "--work" {
            parsed.work = true;
        } else if arg == "--continue" {
            parsed.continue_task = true;
        } else if arg == "--defer-proof" {
            parsed.defer_proof = true;
        } else if let Some(value) = arg.strip_prefix("--context=") {
            parsed.context_policy = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--grant=") {
            parsed.grant_args.push(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--effect-reservation-journal=") {
            parsed.effect_reservation_journal = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--restore=") {
            parsed.restore_arg = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--max-rounds=") {
            parsed.max_rounds = Some(parse_max_rounds(value)?);
        } else if let Some(value) = arg.strip_prefix("--prompt=") {
            if parsed.prompt.is_some() {
                anyhow::bail!("--prompt may be given only once");
            }
            parsed.prompt = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--timeout-secs=") {
            parsed.timeout_secs = Some(parse_timeout_secs(value)?);
        } else if arg.starts_with("--") {
            anyhow::bail!("unknown flag {arg}; see --help");
        } else if parsed.root.is_none() {
            parsed.root = Some(PathBuf::from(arg));
        } else {
            anyhow::bail!(
                "unexpected extra argument {arg:?}; pass one workspace path, or see --help"
            );
        }
    }
    parsed.validate()?;
    Ok(parsed)
}

impl ProductArgs {
    fn validate(&self) -> anyhow::Result<()> {
        if self.help {
            return Ok(());
        }
        if self.read_only && self.restore_arg.is_some() {
            anyhow::bail!("--restore cannot be combined with --read-only");
        }
        if self.read_only && !self.grant_args.is_empty() {
            anyhow::bail!("--grant cannot be combined with --read-only");
        }
        if self.doctor_mode
            && (self.prompt.is_some()
                || self.continue_task
                || self.work
                || self.timeout_secs.is_some())
        {
            anyhow::bail!(
                "--doctor cannot combine with --prompt, --work, --continue, or --timeout-secs"
            );
        }
        if self.work && self.prompt.is_none() {
            anyhow::bail!("--work needs --prompt=<text> (or --prompt=- to read stdin)");
        }
        if self.continue_task && self.prompt.is_some() {
            anyhow::bail!(
                "--continue cannot combine with --prompt: continue replays the stored directive"
            );
        }
        if self.timeout_secs.is_some() && !self.is_headless() {
            anyhow::bail!("--timeout-secs requires --prompt or --continue");
        }
        Ok(())
    }
}

/// Strict `--max-rounds` parsing. The budget counts MODEL rounds — the
/// same unit the runtime enforces (`Failure { RoundBudget }`) and the
/// status banner renders — never tool calls. Zero or garbage is a
/// startup error before any workspace mutation; there is no infinite
/// value: a long task gets an explicitly larger finite budget.
pub fn parse_max_rounds(value: &str) -> anyhow::Result<usize> {
    let rounds: usize = value.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid --max-rounds {value:?}: expected a positive integer (model rounds)"
        )
    })?;
    if rounds == 0 {
        anyhow::bail!("invalid --max-rounds 0: the budget must be at least 1 model round");
    }
    Ok(rounds)
}

fn parse_timeout_secs(value: &str) -> anyhow::Result<u64> {
    let secs: u64 = value.trim().parse().map_err(|_| {
        anyhow::anyhow!("invalid --timeout-secs {value:?}: expected a positive integer")
    })?;
    if secs == 0 || secs > MAX_HEADLESS_TIMEOUT_SECS {
        anyhow::bail!("invalid --timeout-secs {value:?}: expected 1..={MAX_HEADLESS_TIMEOUT_SECS}");
    }
    Ok(secs)
}

pub fn print_usage() {
    print!(
        "\
agent-tui — local coding agent (interactive TUI or headless JSONL)

Usage:
  agent-tui [options] [workspace]
  agent-tui --prompt=<text> [--work] [--grant=<JSON>] [options] [workspace]
  agent-tui --restore=<path|latest> --continue [options] [workspace]
  agent-tui --doctor [workspace]

Headless sessions write runtime events as JSONL on stdout and banners on
stderr. They never prompt for approval. Ungranted writes and process calls
are denied immediately. There is no --yes / --allow-all.

Options:
  --prompt=<text>     one user message (\"-\" reads stdin); implies headless
  --work              compose like /work (set_focus + task.manage + the prompt)
  --continue          continue the restored/active task's stored directive
  --grant=<JSON>      standing write/process grant (repeatable)
  --read-only         deny every write/process call
  --max-rounds=<N>    finite model-round budget for this execution segment
  --timeout-secs=<N>  headless wait cap (default 900, max 86400)
  --restore=<path>    cold resume; \"latest\" resolves in checkpoints/
  --context=<policy>  dynamic | append | rolling | service
  --defer-proof       opt into deferred proof refresh
  --doctor            product self-check and exit
  --help              this text

Exit codes (headless):
  0  turn completed without a denied mutation
  2  round budget reached; --continue starts a new segment
  3  a mutating tool was denied (missing matching --grant)
  1  error, timeout, cancel, or startup failure
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_rounds_parses_strictly_in_model_rounds() {
        assert_eq!(parse_max_rounds("24").unwrap(), 24);
        assert_eq!(parse_max_rounds(" 64 ").unwrap(), 64);
        for bad in ["0", "-4", "abc", "", "2.5", "99999999999999999999"] {
            let error = parse_max_rounds(bad).unwrap_err().to_string();
            assert!(error.contains("--max-rounds"), "{bad}: {error}");
        }
        let zero = parse_max_rounds("0").unwrap_err().to_string();
        assert!(zero.contains("at least 1"), "{zero}");
    }

    #[test]
    fn headless_prompt_is_detected_and_default_timeout_applies() {
        let args = parse_args(["--prompt=fix the parser", "."]).unwrap();
        assert!(args.is_headless());
        assert_eq!(args.prompt.as_deref(), Some("fix the parser"));
        assert_eq!(args.headless_timeout(), DEFAULT_HEADLESS_TIMEOUT);
        assert_eq!(args.root, Some(PathBuf::from(".")));
        assert!(!args.work);
        assert!(!args.continue_task);
    }

    #[test]
    fn work_requires_prompt_and_continue_cannot_combine() {
        let missing = parse_args(["--work"]).unwrap_err().to_string();
        assert!(missing.contains("--work needs --prompt"), "{missing}");

        let mixed = parse_args(["--prompt=go", "--continue"])
            .unwrap_err()
            .to_string();
        assert!(mixed.contains("--continue cannot combine"), "{mixed}");

        let ok = parse_args(["--work", "--prompt=migrate config"]).unwrap();
        assert!(ok.work);
        assert_eq!(ok.prompt.as_deref(), Some("migrate config"));
    }

    #[test]
    fn global_allow_flags_fail_closed() {
        for flag in ["--yes", "--allow-all", "-y"] {
            let error = parse_args([flag]).unwrap_err().to_string();
            assert!(error.contains("allow-all"), "{flag}: {error}");
            assert!(error.contains("--grant="), "{flag}: {error}");
        }
    }

    #[test]
    fn unknown_flags_are_errors_not_workspace_paths() {
        let error = parse_args(["--not-a-flag"]).unwrap_err().to_string();
        assert!(error.contains("unknown flag"), "{error}");
    }

    #[test]
    fn timeout_is_headless_only_and_bounded() {
        let interactive = parse_args(["--timeout-secs=30"]).unwrap_err().to_string();
        assert!(
            interactive.contains("--prompt or --continue"),
            "{interactive}"
        );

        let parsed = parse_args(["--prompt=hi", "--timeout-secs=30"]).unwrap();
        assert_eq!(parsed.timeout_secs, Some(30));

        for bad in ["0", "86401", "nope"] {
            let error = parse_args(["--prompt=hi", &format!("--timeout-secs={bad}")])
                .unwrap_err()
                .to_string();
            assert!(error.contains("--timeout-secs"), "{bad}: {error}");
        }
    }

    #[test]
    fn existing_restore_and_grant_conflicts_still_fail() {
        let restore = parse_args(["--read-only", "--restore=latest"])
            .unwrap_err()
            .to_string();
        assert!(restore.contains("--restore cannot"), "{restore}");
        let grant = parse_args(["--read-only", "--grant={}"])
            .unwrap_err()
            .to_string();
        assert!(grant.contains("--grant cannot"), "{grant}");
    }
}
