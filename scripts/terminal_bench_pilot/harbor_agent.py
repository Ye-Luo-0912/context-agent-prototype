"""Harbor installed-agent adapter for the context-agent pilot.

Harbor owns the task container and verifier.  This adapter only starts the
already-built ``agent-tui`` binary inside the task environment and forwards
the official instruction through stdin.  It intentionally refuses to invent
authorization or a state directory: the image must provide a checked grant
file and the runtime must expose a separate state-dir option before a trial
can be admitted.
"""

from __future__ import annotations

import shlex
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:  # Harbor is optional for local preflight and unit tests.
    from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
except ImportError:  # pragma: no cover - exercised only without Harbor installed.
    class BaseInstalledAgent:  # type: ignore[no-redef]
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            del args
            self._extra_env = dict(kwargs.get("extra_env") or {})

    def with_prompt_template(function):  # type: ignore[no-redef]
        return function


class AdapterConfigurationError(ValueError):
    """The adapter cannot prove that a trial is safe to start."""


PROVIDER_ENV_KEYS = (
    "OPENAI_MODEL",
    "OPENAI_API_PROTOCOL",
    "OPENAI_CHAT_THINKING",
    "OPENAI_RESPONSES_REASONING_EFFORT",
    "OPENAI_MAX_OUTPUT_TOKENS",
    "OPENAI_CONTEXT_WINDOW",
    "OPENAI_BUFFER_STREAM_FOR_RETRY",
    "OPENAI_REQUEST_TIMEOUT_SECS",
    "AGENT_DISABLE_SHELL_EXEC",
)


@dataclass(frozen=True)
class PilotAgentConfig:
    context: str = "rolling"
    task_goal: str = (
        "Solve the official task in /app and preserve every requirement while leaving "
        "the requested implementation in the workspace."
    )
    max_rounds: int = 240
    timeout_secs: int = 7_200
    binary: str = "/opt/context-agent/bin/agent-tui"
    grant_file: str | None = None
    workspace: str = "/app"
    jsonl_out: str = "/tmp/context-agent-run/events.jsonl"
    demo: bool = False
    # Kept explicit so a benchmark run always records where trusted runtime
    # state lives.  The adapter refuses older binaries that lack this flag;
    # silently falling back to a task-root state dir would contaminate
    # official artifacts.
    state_dir: str | None = "/tmp/context-agent-run/state"
    runtime_supports_state_dir: bool = True
    single_file_edit_patches: bool = False

    def validate(self) -> None:
        if self.context not in {"rolling", "dynamic"}:
            raise AdapterConfigurationError(
                f"context must be rolling or dynamic, got {self.context!r}"
            )
        if self.max_rounds <= 0:
            raise AdapterConfigurationError("max_rounds must be positive")
        if self.timeout_secs <= 0:
            raise AdapterConfigurationError("timeout_secs must be positive")
        if not self.binary or not self.workspace or not self.jsonl_out:
            raise AdapterConfigurationError("binary, workspace and jsonl_out are required")
        if not self.grant_file:
            raise AdapterConfigurationError(
                "a checked grant_file is required; the adapter never invents authority"
            )
        if not self.task_goal.strip() or len(self.task_goal) > 1_900:
            raise AdapterConfigurationError(
                "task_goal must be non-empty and below the runtime task-goal cap"
            )
        if self.state_dir and not self.runtime_supports_state_dir:
            raise AdapterConfigurationError(
                "runtime state isolation was requested, but this agent-tui build "
                "does not expose --state-dir"
            )


def build_agent_argv(config: PilotAgentConfig) -> list[str]:
    """Build argv without shell syntax or embedded task text."""

    config.validate()
    argv = [
        config.binary,
        "--prompt=-",
        f"--context={config.context}",
        f"--max-rounds={config.max_rounds}",
        f"--timeout-secs={config.timeout_secs}",
        f"--grant-file={config.grant_file}",
        f"--jsonl-out={config.jsonl_out}",
    ]
    if config.task_goal:
        argv.insert(1, f"--task-goal={config.task_goal}")
    else:
        argv.insert(1, "--work")
    if config.state_dir:
        argv.append(f"--state-dir={config.state_dir}")
    argv.append(config.workspace)
    return argv


def build_prompt_pipeline(instruction: str, config: PilotAgentConfig) -> str:
    """Render a bounded shell pipeline suitable for Harbor's exec API.

    Harbor's installed-agent API accepts a command string rather than a
    separate stdin handle.  ``shlex.quote`` keeps the official instruction
    data, including newlines and shell metacharacters, data-only.
    """

    if not instruction.strip():
        raise AdapterConfigurationError("the official instruction is empty")
    # Describe the actual live-task grants without overriding Core authority.
    instruction = (
        "Harness execution rule: use process.run with explicit argv for commands. "
        "shell.exec is unavailable in this trial; do not load or call it. "
        "Core enforces the checked grant file's write and process scopes. "
        "Use the file tools with workspace-relative paths. "
        "This trial allows writes under api/ and migrate_archive/, and to "
        "entrypoint.sh, migrate.py, and _verify.py at the workspace root. "
        "For edit.patch files[], every target in one call must fit the same "
        "single Core write grant. Patch files in different listed scopes "
        "with separate calls: for example, api/main.py and migrate.py "
        "must not be combined in one edit.patch call even though each "
        "path is individually allowed. "
        "Put any other helper code under api/; do not attempt to write other "
        "top-level paths. "
        "Use fs.list/fs.read for directory inspection; paths are workspace-relative "
        "(`/app` is the empty path), so never prefix file-tool paths with /app. "
        "For process.run, omit cwd so the command runs in /app, and use it only "
        "for bounded python3 commands. The only granted executables are "
        "python3, pytest, ls, curl, and env; never guess or probe other names, "
        "including made-up names such as argv_error_probe. A denied call "
        "stops this trial. "
        "The first argv element must be a non-empty executable name such as "
        "python3; pass the name without quote characters inside the JSON "
        "string, and never use an empty string or a shell fragment there. "
        "python3 may run checks, application code, and -m pip to install declared "
        "task dependencies; it is an execution capability, not a read-only sandbox. "
        "Do not invoke bash, sh, docker, or shell pipelines. "
        "Keep probes and each model response bounded; "
        "never inline large binary/base64 payloads or unbounded command output. "
        "Every tool-call argument must be strict JSON: escape newlines and quotes "
        "and never place NUL or other raw control characters inside a string. "
        "For bounded read-only environment probes, use `python3 /opt/context-agent/bin/read-probe `"
        "with exactly one query from env-names, modules, files, ports, or python; "
        "the broker redacts environment values and does not execute arbitrary code. "
        "Core retains durable completion authority under OperatorClosureOnly. "
        "The separate official verifier grades the workspace after this turn. "
        "Finish the implementation and checks, then report the actual result. "
        "This does not change the task requirements.\n\n"
        + instruction
    )
    if config.single_file_edit_patches:
        instruction = (
            "Trial tool constraint: every edit.patch call must contain exactly "
            "one file in files[]. Never combine files, even when their paths "
            "are both writable. Issue a separate edit.patch call for each file. "
            "The host sends this trial with a strict one-file schema.\n\n"
            + instruction
        )
    argv = " ".join(shlex.quote(part) for part in build_agent_argv(config))
    return (
        f"printf '%s' {shlex.quote(instruction)} | "
        "MAINTENANCE_MAX_CALLS_PER_MAINTAIN=0 "
        "MAINTENANCE_MAX_TOKENS_PER_MAINTAIN=0 "
        f"{argv}"
    )


class ContextAgentTerminalBench(BaseInstalledAgent):
    """Harbor's installed custom-agent entry point.

    The binary and grant file are image inputs.  They are not downloaded from
    the task, the model, or an external URL during a run.
    """

    @staticmethod
    def name() -> str:
        return "context-agent-terminal-bench"

    def __init__(
        self,
        logs_dir: Path | str = "/tmp/context-agent-run/logs",
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        # Harbor scopes --agent-env through BaseInstalledAgent. Forward it so
        # provider settings reach agent-tui without serializing them into the
        # command string or the repository.
        super().__init__(logs_dir=logs_dir, extra_env=extra_env)
        if any(key in self._extra_env for key in ("OPENAI_API_KEY", "OPENAI_BASE_URL")):
            raise AdapterConfigurationError("provider credentials must use the host relay")
        self.config = PilotAgentConfig(
            context=str(kwargs.get("context", "rolling")),
            task_goal=str(kwargs.get("task_goal", PilotAgentConfig().task_goal)),
            max_rounds=int(kwargs.get("max_rounds", 240)),
            timeout_secs=int(kwargs.get("timeout_secs", 7_200)),
            binary=str(kwargs.get("binary", "/opt/context-agent/bin/agent-tui")),
            grant_file=kwargs.get("grant_file"),
            workspace=str(kwargs.get("workspace", "/app")),
            jsonl_out=str(kwargs.get("jsonl_out", "/tmp/context-agent-run/events.jsonl")),
            state_dir=kwargs.get("state_dir", "/tmp/context-agent-run/state"),
            runtime_supports_state_dir=bool(kwargs.get("runtime_supports_state_dir", True)),
            single_file_edit_patches=bool(kwargs.get("single_file_edit_patches", False)),
            demo=str(kwargs.get("demo", "false")).lower() in {"1", "true", "yes"},
        )

    async def install(self, environment) -> None:
        self.config.validate()
        # Do not install packages or fetch binaries in the task container.  A
        # trial image must be built from a pinned binary and checked grant file.
        checks = [
            f"test -x {shlex.quote(self.config.binary)}",
            f"test -f {shlex.quote(self.config.grant_file)}",
            f"{shlex.quote(self.config.binary)} --help | grep -q -- '--state-dir='",
            f"mkdir -p {shlex.quote(self.config.jsonl_out.rsplit('/', 1)[0])}",
        ]
        if self.config.state_dir:
            checks.append(f"mkdir -p {shlex.quote(self.config.state_dir.rsplit('/', 1)[0])}")
        check = " && ".join(checks)
        await self.exec_as_agent(environment, command=check)

    @with_prompt_template
    async def run(self, instruction: str, environment, context=None) -> None:
        del context
        command = build_prompt_pipeline(instruction, self.config)
        env = {
            key: os.environ[key]
            for key in PROVIDER_ENV_KEYS
            if key in os.environ
        }
        if not self.config.demo:
            relay_url = os.environ.get("TB_RELAY_BASE_URL", "")
            relay_token = os.environ.get("TB_RELAY_TOKEN", "")
            if not relay_url.startswith("http://") or not relay_token.startswith("trial-"):
                raise AdapterConfigurationError("a trial-scoped host credential relay is required")
            env["OPENAI_BASE_URL"] = relay_url
            env["OPENAI_API_KEY"] = relay_token
        if self.config.demo:
            env["AGENT_DEMO"] = "1"
        await self.exec_as_agent(environment, command=command, env=env)
