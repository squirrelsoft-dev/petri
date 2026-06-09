"""Sandbox, Commands, CommandResult, SandboxInfo, and option dataclasses.

This module is the main SDK surface. It wraps the ``petri sandbox ...`` CLI
commands and exposes an E2B-style ``Sandbox`` object.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

from petri._cli import Runner, check_cli_result, make_default_runner
from petri.errors import (
    AuthorizationError,
    CommandFailedError,
    CommandTimeoutError,
    NotImplementedInV1Error,
    OutputTruncatedError,
    PolicyDeniedError,
    ProtocolVersionMismatchError,
)

# The only protocol version this client understands.
PROTOCOL_VERSION: int = 1


# ---------------------------------------------------------------------------
# Option dataclasses
# ---------------------------------------------------------------------------


@dataclass
class SandboxCreateOptions:
    """Options for :meth:`Sandbox.create`."""

    workspace: str | None = None
    policy: str | None = None
    id: str | None = None
    backend: str | None = None
    image: str | None = None
    metadata: dict[str, str] = field(default_factory=dict)


@dataclass
class CommandOptions:
    """Options for :meth:`Commands.run`."""

    cwd: str | None = None
    args: list[str] = field(default_factory=list)
    env: dict[str, str] = field(default_factory=dict)
    stdin: str | None = None
    timeout_ms: int | None = None
    max_output_bytes: int | None = None
    request_id: str | None = None


@dataclass
class ListOptions:
    """Options for :meth:`Sandbox.list`."""

    state: str | None = None
    metadata: dict[str, str] = field(default_factory=dict)
    limit: int | None = None


# ---------------------------------------------------------------------------
# SandboxInfo — the lifecycle handle returned by list / get_info
# ---------------------------------------------------------------------------


@dataclass
class SandboxInfo:
    """Lifecycle handle for a sandbox instance, as returned by list/get_info."""

    sandbox_id: str
    state: str
    backend: str
    metadata: dict[str, str] = field(default_factory=dict)

    @classmethod
    def _from_dict(cls, data: dict[str, Any]) -> "SandboxInfo":
        return cls(
            sandbox_id=data["id"],
            state=data.get("state", ""),
            backend=data.get("backend", ""),
            metadata=data.get("metadata") or {},
        )

    def is_running(self) -> bool:
        """Return True when state is ``ready`` or ``running_dispatch``."""
        return self.state in ("ready", "running_dispatch")


# ---------------------------------------------------------------------------
# ErrorFrame — structured error detail inside a ResultFrame
# ---------------------------------------------------------------------------


@dataclass
class ErrorFrame:
    """Structured error detail from a ResultFrame."""

    code: str
    message: str
    details: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def _from_dict(cls, data: dict[str, Any]) -> "ErrorFrame":
        return cls(
            code=data.get("code", ""),
            message=data.get("message", ""),
            details=data.get("details") or {},
        )


# ---------------------------------------------------------------------------
# CommandResult
# ---------------------------------------------------------------------------


@dataclass
class CommandResult:
    """Typed result of :meth:`Commands.run`.

    ``run()`` never raises for non-success statuses or non-zero exit codes —
    those are normal results. Call :meth:`raise_for_status` to opt into
    exceptions on policy-denied / timeout / truncation / command failure.
    """

    status: str
    exit_code: int | None
    stdout: str
    stderr: str
    output_truncated: bool
    error: ErrorFrame | None

    @property
    def success(self) -> bool:
        """True when status is 'success' *and* exit_code is 0."""
        return self.status == "success" and self.exit_code == 0

    def raise_for_status(self) -> "CommandResult":
        """Raise a typed error if the result is not a clean success.

        Checks in this order (per contract):

        1. Protocol version mismatch — handled before this point; included
           for completeness.
        2. :class:`~petri.errors.PolicyDeniedError` — status ``rejected`` or
           ``error.code == "policy_denied"``.
        3. :class:`~petri.errors.CommandTimeoutError` — status ``timeout``.
        4. :class:`~petri.errors.CommandFailedError` — status ``failure``.
        5. :class:`~petri.errors.OutputTruncatedError` — output_truncated True.

        Returns *self* on clean success so callers can chain::

            result = sandbox.commands.run("ls").raise_for_status()
        """
        # Policy-denied (rejected status or policy_denied code)
        if self.status == "rejected" or (
            self.error is not None and self.error.code == "policy_denied"
        ):
            msg = (
                self.error.message
                if self.error
                else "Command was rejected by policy"
            )
            raise PolicyDeniedError(msg)

        # Timeout
        if self.status == "timeout":
            msg = (
                self.error.message if self.error else "Command timed out"
            )
            raise CommandTimeoutError(msg)

        # Command failure (non-zero exit)
        if self.status == "failure":
            msg = f"Command failed with exit code {self.exit_code}"
            raise CommandFailedError(msg, exit_code=self.exit_code)

        # Output truncated (last — can co-exist with success status)
        if self.output_truncated:
            raise OutputTruncatedError(
                "Command output was truncated (max_output_bytes exceeded)"
            )

        return self


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


class Commands:
    """The ``commands`` module: run shell commands inside a sandbox."""

    def __init__(self, sandbox: "Sandbox") -> None:
        self._sandbox = sandbox

    def run(
        self,
        command: str,
        *,
        cwd: str | None = None,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
        stdin: str | None = None,
        timeout_ms: int | None = None,
        max_output_bytes: int | None = None,
        request_id: str | None = None,
    ) -> CommandResult:
        """Run *command* inside the sandbox and return a typed result.

        Non-success statuses and non-zero exit codes are **not** raised — they
        are returned as-is in the :class:`CommandResult`. Call
        :meth:`CommandResult.raise_for_status` to opt into exceptions.

        Args:
            command: The command to execute.
            cwd: Working directory inside the sandbox.
            args: Extra positional arguments appended after the command.
            env: Environment variable overrides (``k=v`` pairs).
            stdin: Text piped to the command's stdin.
            timeout_ms: Per-request wall-clock timeout in milliseconds.
            max_output_bytes: Maximum captured output bytes before truncation.
            request_id: Explicit request id for correlation.

        Returns:
            :class:`CommandResult` with the parsed frame.

        Raises:
            ProtocolVersionMismatchError: ``frame.protocol_version != 1``.
            PetriError: Transport/lifecycle failure (binary missing, JSON
                parse error, non-zero CLI exit).
        """
        argv = self._build_exec_argv(
            command,
            cwd=cwd,
            args=args or [],
            env=env or {},
            timeout_ms=timeout_ms,
            max_output_bytes=max_output_bytes,
            request_id=request_id,
        )

        stdin_bytes = stdin.encode("utf-8") if stdin is not None else None
        stdout_text, stderr_text, returncode = self._sandbox._runner(
            argv, stdin_bytes
        )
        check_cli_result(stdout_text, stderr_text, returncode)

        try:
            frame = json.loads(stdout_text)
        except json.JSONDecodeError as exc:
            from petri.errors import PetriError
            raise PetriError(
                f"Failed to parse ResultFrame JSON: {exc}"
            ) from exc

        # Validate protocol version before anything else.
        proto = frame.get("protocol_version")
        if proto != PROTOCOL_VERSION:
            raise ProtocolVersionMismatchError(proto)

        error_raw = frame.get("error")
        error_frame = ErrorFrame._from_dict(error_raw) if error_raw else None

        # Check for auth/capability errors embedded in the error frame.
        if error_frame is not None and error_frame.code in (
            "capability_denied",
            "authorization_denied",
        ):
            raise AuthorizationError(error_frame.message)

        return CommandResult(
            status=frame.get("status", ""),
            exit_code=frame.get("exit_code"),
            stdout=frame.get("stdout") or "",
            stderr=frame.get("stderr") or "",
            output_truncated=frame.get("output_truncated") or False,
            error=error_frame,
        )

    def _build_exec_argv(
        self,
        command: str,
        *,
        cwd: str | None,
        args: list[str],
        env: dict[str, str],
        timeout_ms: int | None,
        max_output_bytes: int | None,
        request_id: str | None,
    ) -> list[str]:
        """Build the ``sandbox exec`` argv (without the binary name)."""
        argv: list[str] = ["sandbox", "exec", self._sandbox.sandbox_id]

        if cwd is not None:
            argv += ["--cwd", cwd]
        if env:
            argv += ["--env", ",".join(f"{k}={v}" for k, v in env.items())]
        if timeout_ms is not None:
            argv += ["--timeout-ms", str(timeout_ms)]
        if max_output_bytes is not None:
            argv += ["--max-output-bytes", str(max_output_bytes)]
        if request_id is not None:
            argv += ["--request-id", request_id]

        # Stop option parsing, then the command and any extra args.
        argv += ["--", command, *args]
        return argv


# ---------------------------------------------------------------------------
# Sandbox
# ---------------------------------------------------------------------------


class Sandbox:
    """A handle to a single Petri sandbox.

    Create or connect via the classmethods; use the instance for commands
    and lifecycle operations.
    """

    def __init__(
        self,
        sandbox_id: str,
        *,
        runner: Runner,
    ) -> None:
        self._sandbox_id = sandbox_id
        self._runner = runner
        self._commands = Commands(self)

    # ------------------------------------------------------------------
    # Properties
    # ------------------------------------------------------------------

    @property
    def sandbox_id(self) -> str:
        """The sandbox's unique identifier."""
        return self._sandbox_id

    @property
    def commands(self) -> Commands:
        """The ``commands`` module for this sandbox."""
        return self._commands

    @property
    def files(self) -> None:
        """Reserved — not implemented in v1."""
        raise NotImplementedInV1Error("files")

    @property
    def git(self) -> None:
        """Reserved — not implemented in v1."""
        raise NotImplementedInV1Error("git")

    @property
    def pty(self) -> None:
        """Reserved — not implemented in v1."""
        raise NotImplementedInV1Error("pty")

    # ------------------------------------------------------------------
    # Instance lifecycle
    # ------------------------------------------------------------------
    # NOTE: ``kill`` is injected by ``_KillDescriptor`` after the class body
    # so it can act as *both* a classmethod (``Sandbox.kill("id")``) and an
    # instance method (``sandbox.kill()``). Do not define ``kill`` here.

    def get_info(
        self,
        *,
        petri_path: str | None = None,
        runner: Runner | None = None,
    ) -> SandboxInfo | None:
        """Return the current lifecycle handle, or ``None`` if not found."""
        r = runner or self._runner
        handles = Sandbox.list(runner=r, petri_path=petri_path)
        for h in handles:
            if h.sandbox_id == self._sandbox_id:
                return h
        return None

    def is_running(
        self,
        *,
        petri_path: str | None = None,
        runner: Runner | None = None,
    ) -> bool:
        """Return True when the sandbox state is ``ready`` or
        ``running_dispatch``.
        """
        info = self.get_info(petri_path=petri_path, runner=runner)
        return info is not None and info.is_running()

    # ------------------------------------------------------------------
    # Static / class methods
    # ------------------------------------------------------------------

    @classmethod
    def create(
        cls,
        template: str = "base",
        *,
        workspace: str | None = None,
        policy: str | None = None,
        id: str | None = None,
        backend: str | None = None,
        image: str | None = None,
        metadata: dict[str, str] | None = None,
        petri_path: str | None = None,
        runner: Runner | None = None,
    ) -> "Sandbox":
        """Create a new sandbox and return a handle to it.

        Args:
            template: Template name (defaults to ``"base"``).
            workspace: Host workspace directory mounted into the sandbox.
            policy: Policy file path applied at boot.
            id: Explicit sandbox id (generated when omitted).
            backend: Backend name (defaults to ``"macos"``).
            image: Image bundle path (defaults to backend default).
            metadata: Free-form metadata persisted with the instance.
            petri_path: Explicit path to the ``petri`` binary.
            runner: Injectable runner (used by tests instead of a real binary).

        Returns:
            :class:`Sandbox` handle.
        """
        r = runner or make_default_runner(petri_path)
        argv = _build_create_argv(
            template,
            workspace=workspace,
            policy=policy,
            id=id,
            backend=backend,
            image=image,
            metadata=metadata,
        )
        stdout_text, stderr_text, returncode = r(argv)
        check_cli_result(stdout_text, stderr_text, returncode)
        sandbox_id = stdout_text.strip()
        return cls(sandbox_id, runner=r)

    @classmethod
    def connect(
        cls,
        sandbox_id: str,
        *,
        petri_path: str | None = None,
        runner: Runner | None = None,
    ) -> "Sandbox":
        """Attach to an existing running sandbox.

        Raises :class:`~petri.errors.SandboxNotFoundError` if missing,
        :class:`~petri.errors.SandboxNotReadyError` if not running.
        """
        r = runner or make_default_runner(petri_path)
        argv = ["sandbox", "connect", sandbox_id]
        stdout_text, stderr_text, returncode = r(argv)
        check_cli_result(stdout_text, stderr_text, returncode)
        return cls(sandbox_id, runner=r)

    @classmethod
    def list(
        cls,
        *,
        state: str | None = None,
        metadata: dict[str, str] | None = None,
        limit: int | None = None,
        petri_path: str | None = None,
        runner: Runner | None = None,
    ) -> list[SandboxInfo]:
        """List sandboxes known to the backend.

        Args:
            state: Filter by lifecycle state (e.g. ``"running"``).
            metadata: Filter by metadata key/value pairs.
            limit: Maximum number of results.
            petri_path: Explicit path to the ``petri`` binary.
            runner: Injectable runner.

        Returns:
            List of :class:`SandboxInfo` handles.
        """
        r = runner or make_default_runner(petri_path)
        argv: list[str] = ["sandbox", "list", "--format", "json"]
        if state is not None:
            argv += ["--state", state]
        if metadata:
            argv += ["--metadata", ",".join(f"{k}={v}" for k, v in metadata.items())]
        if limit is not None:
            argv += ["--limit", str(limit)]

        stdout_text, stderr_text, returncode = r(argv)
        check_cli_result(stdout_text, stderr_text, returncode)

        try:
            raw = json.loads(stdout_text)
        except json.JSONDecodeError as exc:
            from petri.errors import PetriError
            raise PetriError(
                f"Failed to parse sandbox list JSON: {exc}"
            ) from exc

        return [SandboxInfo._from_dict(item) for item in raw]

    # ``kill`` is defined below the class body as a _KillDescriptor so it can
    # behave both as a classmethod (Sandbox.kill("id")) and as an instance
    # method (sandbox.kill()). See ``_KillDescriptor`` for implementation.

    @classmethod
    def kill_id(
        cls,
        sandbox_id: str,
        *,
        petri_path: str | None = None,
        runner: Runner | None = None,
    ) -> None:
        """Kill a sandbox by id without holding a handle to it.

        Alias kept for callers that prefer an explicit id.
        """
        _kill_sandbox(sandbox_id, petri_path=petri_path, runner=runner)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _kill_sandbox(
    sandbox_id: str,
    *,
    petri_path: str | None = None,
    runner: Runner | None = None,
) -> None:
    """Module-level helper used by both the instance and class kill methods."""
    r = runner or make_default_runner(petri_path)
    argv = ["sandbox", "kill", sandbox_id]
    stdout_text, stderr_text, returncode = r(argv)
    check_cli_result(stdout_text, stderr_text, returncode)


class _KillDescriptor:
    """Descriptor that makes ``Sandbox.kill`` work as both a classmethod and
    an instance method.

    - ``Sandbox.kill(sandbox_id, ...)``  — class-level static kill
    - ``sandbox.kill(...)``              — kills this instance's sandbox
    """

    def __get__(
        self, obj: "Sandbox | None", objtype: type | None = None
    ) -> Any:
        if obj is None:
            # Called on the class: Sandbox.kill("sb-1", ...)
            def _class_kill(
                sandbox_id: str,
                *,
                petri_path: str | None = None,
                runner: Runner | None = None,
            ) -> None:
                _kill_sandbox(sandbox_id, petri_path=petri_path, runner=runner)

            return _class_kill
        else:
            # Called on an instance: sandbox.kill(...)
            def _instance_kill(
                *,
                petri_path: str | None = None,
                runner: Runner | None = None,
            ) -> None:
                _kill_sandbox(
                    obj._sandbox_id,
                    petri_path=petri_path,
                    runner=runner or obj._runner,
                )

            return _instance_kill


Sandbox.kill = _KillDescriptor()  # type: ignore[attr-defined]


def _build_create_argv(
    template: str,
    *,
    workspace: str | None,
    policy: str | None,
    id: str | None,
    backend: str | None,
    image: str | None,
    metadata: dict[str, str] | None,
) -> list[str]:
    """Build the ``sandbox create`` argv (without the binary name)."""
    argv: list[str] = ["sandbox", "create", template]

    if workspace is not None:
        argv += ["--workspace", workspace]
    if policy is not None:
        argv += ["--policy", policy]
    if id is not None:
        argv += ["--id", id]
    if backend is not None:
        argv += ["--backend", backend]
    if image is not None:
        argv += ["--image", image]
    if metadata:
        argv += ["--metadata", ",".join(f"{k}={v}" for k, v in metadata.items())]

    return argv
