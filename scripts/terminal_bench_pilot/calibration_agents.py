"""Supplier-free calibration agents used only for Harbor preflight.

`NoopAgent` intentionally does not inspect the task or write `/app`.  Its
expected reward is zero; a nonzero reward means the verifier or artifact
boundary is not testing the intended baseline.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

try:
    from harbor.agents.installed.base import BaseInstalledAgent
except ImportError:  # pragma: no cover - Harbor is present in the calibration venv.
    BaseInstalledAgent = object  # type: ignore[assignment,misc]


class NoopAgent(BaseInstalledAgent):
    @staticmethod
    def name() -> str:
        return "context-agent-terminal-bench-noop"

    def __init__(self, logs_dir: Path | str = "/tmp/context-agent-noop/logs", **kwargs):
        del kwargs
        super().__init__(logs_dir=logs_dir)

    async def install(self, environment) -> None:
        del environment

    async def run(self, instruction: str, environment, context=None) -> None:
        del instruction, context
        await self.exec_as_agent(environment, command="true")


class StaticVariantAgent(BaseInstalledAgent):
    """Install a checked local fault-variant patch without a model call.

    This agent is deliberately supplier-free.  It uploads only the declared
    files into the task's existing ``/app/api`` tree, then lets Harbor collect
    and verify the resulting artifact.  It is for independent fault variants,
    never for normal benchmark arms.
    """

    @staticmethod
    def name() -> str:
        return "context-agent-terminal-bench-static-variant"

    def __init__(
        self,
        logs_dir: Path | str = "/tmp/context-agent-static-variant/logs",
        source_dir: str | None = None,
        files: list[str] | None = None,
        **kwargs: Any,
    ) -> None:
        del kwargs
        super().__init__(logs_dir=logs_dir)
        if not source_dir:
            raise ValueError("source_dir is required")
        self.source_dir = Path(source_dir).resolve()
        self.files = tuple(files or ())
        if not self.files:
            raise ValueError("files is required")
        if any(Path(rel).is_absolute() or ".." in Path(rel).parts for rel in self.files):
            raise ValueError("files must be relative and confined to source_dir")

    async def install(self, environment) -> None:
        for rel in self.files:
            source = (self.source_dir / rel).resolve()
            if self.source_dir not in source.parents:
                raise ValueError(f"variant file escapes source_dir: {rel}")
            await environment.upload_file(source, f"/app/api/{rel}")

    async def run(self, instruction: str, environment, context=None) -> None:
        del instruction, context
        await self.exec_as_agent(environment, command="true")


class MigrationVariantAgent(StaticVariantAgent):
    """Static fault variant that runs the checked migration in the main service."""

    @staticmethod
    def name() -> str:
        return "context-agent-terminal-bench-migration-variant"

    async def run(self, instruction: str, environment, context=None) -> None:
        del instruction, context
        await self.exec_as_root(
            environment,
            command=(
                "env -u PYTHONPATH pip install --no-cache-dir "
                "-r /app/api/requirements.txt"
            ),
            timeout_sec=900,
        )
        await self.exec_as_root(
            environment,
            command=(
                "cd /app && env -u PYTHONPATH python3 -c "
                "\"from api.db import create_postgres_engine; "
                "e=create_postgres_engine(); e.dispose(); print('migration_complete')\""
            ),
            timeout_sec=3600,
        )


class DataMigrationVariantAgent(StaticVariantAgent):
    """Upload a checked API variant and run one external migration coordinator."""

    @staticmethod
    def name() -> str:
        return "context-agent-terminal-bench-data-migration-variant"

    def __init__(self, migration_script: str | None = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        if not migration_script:
            raise ValueError("migration_script is required")
        self.migration_script = Path(migration_script).resolve()

    async def install(self, environment) -> None:
        await super().install(environment)
        await environment.upload_file(self.migration_script, "/tmp/live-migrate.py")

    async def run(self, instruction: str, environment, context=None) -> None:
        del instruction, context
        await self.exec_as_root(
            environment,
            command=(
                "env -u PYTHONPATH pip install --no-cache-dir "
                "-r /app/api/requirements.txt"
            ),
            timeout_sec=900,
        )
        await self.exec_as_root(
            environment,
            command="env -u PYTHONPATH python3 /tmp/live-migrate.py",
            timeout_sec=3_600,
        )
