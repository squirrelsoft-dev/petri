"""Thorough unit tests for the Petri Python client.

All tests use a FakeRunner that never spawns the real binary. The fake runner
is injected via the ``runner=`` keyword argument on every SDK call.
"""

from __future__ import annotations

import json
import unittest
from typing import Any

# Absolute import from the package root (tests/ sits next to petri/).
import sys
import os

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from petri import (
    PROTOCOL_VERSION,
    CommandResult,
    Sandbox,
    SandboxInfo,
)
from petri.errors import (
    CommandFailedError,
    CommandTimeoutError,
    NotImplementedInV1Error,
    OutputTruncatedError,
    PetriError,
    PolicyDeniedError,
    ProtocolVersionMismatchError,
    SandboxNotFoundError,
    SandboxNotReadyError,
)


# ---------------------------------------------------------------------------
# Helpers / FakeRunner
# ---------------------------------------------------------------------------


class FakeRunner:
    """A fake runner that returns pre-programmed (stdout, stderr, returncode).

    Stores the last argv/stdin for inspection.
    """

    def __init__(
        self,
        stdout: str = "",
        stderr: str = "",
        returncode: int = 0,
    ) -> None:
        self.stdout = stdout
        self.stderr = stderr
        self.returncode = returncode
        self.calls: list[tuple[list[str], bytes | None]] = []

    def __call__(
        self, argv: list[str], stdin: bytes | None = None
    ) -> tuple[str, str, int]:
        self.calls.append((argv, stdin))
        return self.stdout, self.stderr, self.returncode

    @property
    def last_argv(self) -> list[str]:
        assert self.calls, "No calls recorded"
        return self.calls[-1][0]

    @property
    def last_stdin(self) -> bytes | None:
        assert self.calls, "No calls recorded"
        return self.calls[-1][1]


def _make_result_frame(
    status: str = "success",
    exit_code: int | None = 0,
    stdout_text: str = "hello\n",
    stderr_text: str = "",
    output_truncated: bool = False,
    error: dict[str, Any] | None = None,
    protocol_version: int = 1,
) -> str:
    """Build a JSON ResultFrame string."""
    frame: dict[str, Any] = {
        "protocol_version": protocol_version,
        "id": "test-req-1",
        "status": status,
        "elapsed_ms": 1,
        "stdout": stdout_text,
        "stderr": stderr_text,
        "output_truncated": output_truncated,
    }
    if exit_code is not None:
        frame["exit_code"] = exit_code
    if error is not None:
        frame["error"] = error
    return json.dumps(frame)


def _make_sandbox(sandbox_id: str = "sb-1", runner: FakeRunner | None = None) -> Sandbox:
    """Create a Sandbox instance directly (bypassing the CLI create)."""
    from petri.sandbox import Sandbox as _Sandbox

    r = runner or FakeRunner()
    return _Sandbox(sandbox_id, runner=r)


# ---------------------------------------------------------------------------
# create() argv mapping
# ---------------------------------------------------------------------------


class TestCreate(unittest.TestCase):
    def test_minimal_create_argv(self) -> None:
        """create() with only a template produces the minimal argv."""
        runner = FakeRunner(stdout="sb-42\n")
        Sandbox.create("base", runner=runner)
        argv = runner.last_argv
        self.assertEqual(argv[:3], ["sandbox", "create", "base"])
        self.assertNotIn("--id", argv)
        self.assertNotIn("--backend", argv)
        self.assertNotIn("--image", argv)
        self.assertNotIn("--metadata", argv)

    def test_create_with_workspace_and_policy(self) -> None:
        runner = FakeRunner(stdout="sb-1\n")
        Sandbox.create("base", workspace="/ws", policy="./pol.toml", runner=runner)
        argv = runner.last_argv
        self.assertIn("--workspace", argv)
        idx = argv.index("--workspace")
        self.assertEqual(argv[idx + 1], "/ws")
        self.assertIn("--policy", argv)
        idx = argv.index("--policy")
        self.assertEqual(argv[idx + 1], "./pol.toml")

    def test_create_with_optional_flags(self) -> None:
        """--id, --backend, and --image are only included when set."""
        runner = FakeRunner(stdout="my-id\n")
        Sandbox.create(
            "base",
            id="my-id",
            backend="macos",
            image="/path/to/image",
            runner=runner,
        )
        argv = runner.last_argv
        self.assertIn("--id", argv)
        self.assertEqual(argv[argv.index("--id") + 1], "my-id")
        self.assertIn("--backend", argv)
        self.assertEqual(argv[argv.index("--backend") + 1], "macos")
        self.assertIn("--image", argv)
        self.assertEqual(argv[argv.index("--image") + 1], "/path/to/image")

    def test_create_metadata_comma_joined(self) -> None:
        """--metadata is a single comma-joined k=v string."""
        runner = FakeRunner(stdout="sb-1\n")
        Sandbox.create("base", metadata={"env": "prod", "ver": "2"}, runner=runner)
        argv = runner.last_argv
        self.assertIn("--metadata", argv)
        meta_val = argv[argv.index("--metadata") + 1]
        # Both pairs must be present (order may vary by dict)
        self.assertIn("env=prod", meta_val)
        self.assertIn("ver=2", meta_val)

    def test_create_default_template(self) -> None:
        """When no template is supplied it defaults to 'base'."""
        runner = FakeRunner(stdout="sb-1\n")
        Sandbox.create(runner=runner)
        argv = runner.last_argv
        self.assertEqual(argv[2], "base")

    def test_create_returns_sandbox_with_stripped_id(self) -> None:
        runner = FakeRunner(stdout="  sb-99  \n")
        sb = Sandbox.create(runner=runner)
        self.assertEqual(sb.sandbox_id, "sb-99")

    def test_create_propagates_cli_error(self) -> None:
        runner = FakeRunner(stderr="petri: some error", returncode=1)
        with self.assertRaises(PetriError):
            Sandbox.create(runner=runner)


# ---------------------------------------------------------------------------
# connect()
# ---------------------------------------------------------------------------


class TestConnect(unittest.TestCase):
    def test_connect_argv(self) -> None:
        runner = FakeRunner()
        Sandbox.connect("sb-1", runner=runner)
        self.assertEqual(runner.last_argv, ["sandbox", "connect", "sb-1"])

    def test_connect_returns_sandbox(self) -> None:
        runner = FakeRunner()
        sb = Sandbox.connect("sb-1", runner=runner)
        self.assertEqual(sb.sandbox_id, "sb-1")

    def test_connect_not_found(self) -> None:
        runner = FakeRunner(
            stderr="petri: no sandbox with id 'sb-1'", returncode=1
        )
        with self.assertRaises(SandboxNotFoundError):
            Sandbox.connect("sb-1", runner=runner)

    def test_connect_not_running(self) -> None:
        runner = FakeRunner(
            stderr="petri: sandbox 'sb-1' is not running (state: torn_down)",
            returncode=1,
        )
        with self.assertRaises(SandboxNotReadyError):
            Sandbox.connect("sb-1", runner=runner)


# ---------------------------------------------------------------------------
# kill()
# ---------------------------------------------------------------------------


class TestKill(unittest.TestCase):
    def test_kill_static_argv(self) -> None:
        runner = FakeRunner()
        Sandbox.kill("sb-1", runner=runner)
        self.assertEqual(runner.last_argv, ["sandbox", "kill", "sb-1"])

    def test_kill_instance(self) -> None:
        runner = FakeRunner()
        sb = _make_sandbox("sb-1", runner=runner)
        sb.kill(runner=runner)
        self.assertEqual(runner.last_argv, ["sandbox", "kill", "sb-1"])

    def test_kill_propagates_not_found(self) -> None:
        runner = FakeRunner(
            stderr="petri: no sandbox with id 'sb-1'", returncode=1
        )
        with self.assertRaises(SandboxNotFoundError):
            Sandbox.kill("sb-1", runner=runner)


# ---------------------------------------------------------------------------
# list()
# ---------------------------------------------------------------------------


class TestList(unittest.TestCase):
    _sample = json.dumps(
        [
            {"id": "sb-1", "state": "ready", "backend": "macos", "metadata": {}},
            {
                "id": "sb-2",
                "state": "torn_down",
                "backend": "macos",
                "metadata": {"env": "prod"},
            },
        ]
    )

    def test_list_basic_argv(self) -> None:
        runner = FakeRunner(stdout=self._sample)
        Sandbox.list(runner=runner)
        self.assertEqual(
            runner.last_argv[:4], ["sandbox", "list", "--format", "json"]
        )

    def test_list_parses_json(self) -> None:
        runner = FakeRunner(stdout=self._sample)
        handles = Sandbox.list(runner=runner)
        self.assertEqual(len(handles), 2)
        self.assertIsInstance(handles[0], SandboxInfo)
        self.assertEqual(handles[0].sandbox_id, "sb-1")
        self.assertEqual(handles[0].state, "ready")
        self.assertEqual(handles[1].metadata, {"env": "prod"})

    def test_list_with_state_filter(self) -> None:
        runner = FakeRunner(stdout="[]")
        Sandbox.list(state="running", runner=runner)
        argv = runner.last_argv
        self.assertIn("--state", argv)
        self.assertEqual(argv[argv.index("--state") + 1], "running")

    def test_list_with_metadata_filter(self) -> None:
        runner = FakeRunner(stdout="[]")
        Sandbox.list(metadata={"env": "prod"}, runner=runner)
        argv = runner.last_argv
        self.assertIn("--metadata", argv)
        meta_val = argv[argv.index("--metadata") + 1]
        self.assertIn("env=prod", meta_val)

    def test_list_with_limit(self) -> None:
        runner = FakeRunner(stdout="[]")
        Sandbox.list(limit=5, runner=runner)
        argv = runner.last_argv
        self.assertIn("--limit", argv)
        self.assertEqual(argv[argv.index("--limit") + 1], "5")

    def test_list_invalid_json_raises(self) -> None:
        runner = FakeRunner(stdout="not-json")
        with self.assertRaises(PetriError):
            Sandbox.list(runner=runner)


# ---------------------------------------------------------------------------
# commands.run() — success path
# ---------------------------------------------------------------------------


class TestCommandsRunSuccess(unittest.TestCase):
    def _run(self, frame: str, **kwargs: Any) -> tuple[CommandResult, FakeRunner]:
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        result = sb.commands.run("ls", **kwargs)
        return result, runner

    def test_success_result(self) -> None:
        frame = _make_result_frame(status="success", exit_code=0, stdout_text="ok\n")
        result, _ = self._run(frame)
        self.assertEqual(result.status, "success")
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, "ok\n")
        self.assertTrue(result.success)

    def test_exec_argv_structure(self) -> None:
        """argv must be: sandbox exec <id> [flags] -- <cmd> [args]"""
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("ls", cwd="/tmp", args=["-la"])
        argv = runner.last_argv
        self.assertEqual(argv[0], "sandbox")
        self.assertEqual(argv[1], "exec")
        self.assertEqual(argv[2], "sb-1")
        # '--' must appear, followed by the command
        self.assertIn("--", argv)
        sep = argv.index("--")
        self.assertEqual(argv[sep + 1], "ls")
        self.assertEqual(argv[sep + 2], "-la")

    def test_cwd_flag(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("ls", cwd="/custom")
        argv = runner.last_argv
        self.assertIn("--cwd", argv)
        self.assertEqual(argv[argv.index("--cwd") + 1], "/custom")

    def test_env_flag(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("env", env={"FOO": "bar"})
        argv = runner.last_argv
        self.assertIn("--env", argv)
        env_val = argv[argv.index("--env") + 1]
        self.assertIn("FOO=bar", env_val)

    def test_timeout_ms_flag(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("sleep", timeout_ms=5000)
        argv = runner.last_argv
        self.assertIn("--timeout-ms", argv)
        self.assertEqual(argv[argv.index("--timeout-ms") + 1], "5000")

    def test_max_output_bytes_flag(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("cat", max_output_bytes=1024)
        argv = runner.last_argv
        self.assertIn("--max-output-bytes", argv)
        self.assertEqual(argv[argv.index("--max-output-bytes") + 1], "1024")

    def test_request_id_flag(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("echo", request_id="req-abc")
        argv = runner.last_argv
        self.assertIn("--request-id", argv)
        self.assertEqual(argv[argv.index("--request-id") + 1], "req-abc")

    def test_stdin_piped(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("cat", stdin="hello world")
        self.assertEqual(runner.last_stdin, b"hello world")

    def test_no_stdin_passes_none(self) -> None:
        frame = _make_result_frame()
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        sb.commands.run("echo")
        self.assertIsNone(runner.last_stdin)

    def test_output_truncated_field(self) -> None:
        frame = _make_result_frame(output_truncated=True)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        result = sb.commands.run("cat")
        self.assertTrue(result.output_truncated)


# ---------------------------------------------------------------------------
# commands.run() — non-zero exit is a result, NOT an exception
# ---------------------------------------------------------------------------


class TestCommandsRunNonZeroExit(unittest.TestCase):
    def test_non_zero_exit_returns_result(self) -> None:
        """A non-zero exit code must NOT raise — it returns a CommandResult."""
        frame = _make_result_frame(
            status="failure",
            exit_code=1,
            stderr_text="error: something went wrong\n",
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        result = sb.commands.run("false")
        self.assertIsInstance(result, CommandResult)
        self.assertFalse(result.success)
        self.assertEqual(result.exit_code, 1)
        self.assertEqual(result.status, "failure")

    def test_success_false_when_exit_code_nonzero(self) -> None:
        frame = _make_result_frame(status="success", exit_code=2)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        result = sb.commands.run("true")
        self.assertFalse(result.success)

    def test_success_false_when_status_failure(self) -> None:
        frame = _make_result_frame(status="failure", exit_code=0)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox("sb-1", runner=runner)
        result = sb.commands.run("cmd")
        self.assertFalse(result.success)


# ---------------------------------------------------------------------------
# raise_for_status() — PolicyDeniedError
# ---------------------------------------------------------------------------


class TestRaiseForStatusPolicy(unittest.TestCase):
    def test_raises_policy_denied_on_rejected_status(self) -> None:
        frame = _make_result_frame(
            status="rejected",
            exit_code=None,
            error={
                "code": "policy_denied",
                "message": "command is not allowed",
                "details": {},
            },
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("curl")
        with self.assertRaises(PolicyDeniedError):
            result.raise_for_status()

    def test_raises_policy_denied_on_policy_code_regardless_of_status(self) -> None:
        frame = _make_result_frame(
            status="malformed",  # unusual but error.code wins
            exit_code=None,
            error={"code": "policy_denied", "message": "blocked", "details": {}},
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("curl")
        with self.assertRaises(PolicyDeniedError):
            result.raise_for_status()


# ---------------------------------------------------------------------------
# raise_for_status() — CommandTimeoutError
# ---------------------------------------------------------------------------


class TestRaiseForStatusTimeout(unittest.TestCase):
    def test_raises_timeout(self) -> None:
        frame = _make_result_frame(
            status="timeout",
            exit_code=None,
            error={"code": "timeout", "message": "timed out", "details": {}},
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("sleep")
        with self.assertRaises(CommandTimeoutError):
            result.raise_for_status()


# ---------------------------------------------------------------------------
# raise_for_status() — CommandFailedError
# ---------------------------------------------------------------------------


class TestRaiseForStatusCommandFailed(unittest.TestCase):
    def test_raises_command_failed(self) -> None:
        frame = _make_result_frame(status="failure", exit_code=2)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("false")
        with self.assertRaises(CommandFailedError) as ctx:
            result.raise_for_status()
        self.assertEqual(ctx.exception.exit_code, 2)

    def test_policy_denied_checked_before_failure(self) -> None:
        """Contract ordering: rejected → timeout → failure → truncated."""
        frame = _make_result_frame(
            status="rejected",
            exit_code=1,
            error={"code": "policy_denied", "message": "blocked", "details": {}},
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("cmd")
        with self.assertRaises(PolicyDeniedError):
            result.raise_for_status()


# ---------------------------------------------------------------------------
# raise_for_status() — OutputTruncatedError
# ---------------------------------------------------------------------------


class TestRaiseForStatusTruncated(unittest.TestCase):
    def test_raises_output_truncated(self) -> None:
        frame = _make_result_frame(
            status="success", exit_code=0, output_truncated=True
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("cat")
        with self.assertRaises(OutputTruncatedError):
            result.raise_for_status()

    def test_truncated_checked_last(self) -> None:
        """Truncated is last — failure should win over truncated."""
        frame = _make_result_frame(
            status="failure", exit_code=1, output_truncated=True
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("cmd")
        with self.assertRaises(CommandFailedError):
            result.raise_for_status()

    def test_no_exception_on_clean_success(self) -> None:
        frame = _make_result_frame(status="success", exit_code=0, output_truncated=False)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("ls")
        returned = result.raise_for_status()
        self.assertIs(returned, result)


# ---------------------------------------------------------------------------
# Lifecycle errors from stderr
# ---------------------------------------------------------------------------


class TestStderrLifecycleErrors(unittest.TestCase):
    def test_sandbox_not_found_from_stderr(self) -> None:
        runner = FakeRunner(
            stderr="petri: no sandbox with id 'sb-99'", returncode=1
        )
        sb = _make_sandbox("sb-99", runner=runner)
        with self.assertRaises(SandboxNotFoundError):
            sb.commands.run("ls")

    def test_sandbox_not_ready_from_stderr(self) -> None:
        runner = FakeRunner(
            stderr="petri: sandbox 'sb-1' is not running (state: torn_down)",
            returncode=1,
        )
        sb = _make_sandbox("sb-1", runner=runner)
        with self.assertRaises(SandboxNotReadyError):
            sb.commands.run("ls")

    def test_generic_error_from_stderr(self) -> None:
        runner = FakeRunner(
            stderr="petri: something unexpected happened", returncode=1
        )
        sb = _make_sandbox("sb-1", runner=runner)
        with self.assertRaises(PetriError):
            sb.commands.run("ls")

    def test_prefix_stripped_from_message(self) -> None:
        runner = FakeRunner(
            stderr="petri: no sandbox with id 'sb-1'", returncode=1
        )
        sb = _make_sandbox("sb-1", runner=runner)
        try:
            sb.commands.run("ls")
            self.fail("Expected SandboxNotFoundError")
        except SandboxNotFoundError as exc:
            self.assertNotIn("petri:", str(exc))


# ---------------------------------------------------------------------------
# ProtocolVersionMismatchError
# ---------------------------------------------------------------------------


class TestProtocolVersionMismatch(unittest.TestCase):
    def test_raises_on_version_2(self) -> None:
        frame = _make_result_frame(protocol_version=2)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        with self.assertRaises(ProtocolVersionMismatchError) as ctx:
            sb.commands.run("ls")
        self.assertEqual(ctx.exception.actual, 2)

    def test_raises_on_version_0(self) -> None:
        frame = _make_result_frame(protocol_version=0)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        with self.assertRaises(ProtocolVersionMismatchError):
            sb.commands.run("ls")

    def test_no_error_on_version_1(self) -> None:
        frame = _make_result_frame(protocol_version=1)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("ls")
        self.assertIsInstance(result, CommandResult)


# ---------------------------------------------------------------------------
# Reserved modules: files / git / pty
# ---------------------------------------------------------------------------


class TestReservedModules(unittest.TestCase):
    def _make(self) -> Sandbox:
        return _make_sandbox()

    def test_files_raises(self) -> None:
        sb = self._make()
        with self.assertRaises(NotImplementedInV1Error):
            _ = sb.files

    def test_git_raises(self) -> None:
        sb = self._make()
        with self.assertRaises(NotImplementedInV1Error):
            _ = sb.git

    def test_pty_raises(self) -> None:
        sb = self._make()
        with self.assertRaises(NotImplementedInV1Error):
            _ = sb.pty

    def test_error_message_contains_module_name(self) -> None:
        sb = self._make()
        try:
            _ = sb.files
            self.fail("Expected NotImplementedInV1Error")
        except NotImplementedInV1Error as exc:
            self.assertIn("files", str(exc))

    def test_not_implemented_is_petri_error(self) -> None:
        sb = self._make()
        with self.assertRaises(PetriError):
            _ = sb.git


# ---------------------------------------------------------------------------
# SandboxInfo
# ---------------------------------------------------------------------------


class TestSandboxInfo(unittest.TestCase):
    def test_is_running_ready(self) -> None:
        info = SandboxInfo(sandbox_id="sb-1", state="ready", backend="macos")
        self.assertTrue(info.is_running())

    def test_is_running_running_dispatch(self) -> None:
        info = SandboxInfo(
            sandbox_id="sb-1", state="running_dispatch", backend="macos"
        )
        self.assertTrue(info.is_running())

    def test_is_not_running_torn_down(self) -> None:
        info = SandboxInfo(sandbox_id="sb-1", state="torn_down", backend="macos")
        self.assertFalse(info.is_running())


# ---------------------------------------------------------------------------
# Miscellaneous edge cases
# ---------------------------------------------------------------------------


class TestMisc(unittest.TestCase):
    def test_malformed_json_raises_petri_error(self) -> None:
        runner = FakeRunner(stdout="definitely not json", returncode=0)
        sb = _make_sandbox(runner=runner)
        with self.assertRaises(PetriError):
            sb.commands.run("ls")

    def test_result_error_field_none_on_success(self) -> None:
        frame = _make_result_frame(status="success", exit_code=0)
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("ls")
        self.assertIsNone(result.error)

    def test_result_error_frame_parsed(self) -> None:
        frame = _make_result_frame(
            status="rejected",
            exit_code=None,
            error={
                "code": "policy_denied",
                "message": "blocked command",
                "details": {"command": "curl"},
            },
        )
        runner = FakeRunner(stdout=frame)
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("curl")
        self.assertIsNotNone(result.error)
        assert result.error is not None
        self.assertEqual(result.error.code, "policy_denied")
        self.assertEqual(result.error.message, "blocked command")
        self.assertEqual(result.error.details, {"command": "curl"})

    def test_stdout_defaults_to_empty_string_when_missing(self) -> None:
        frame: dict[str, Any] = {
            "protocol_version": 1,
            "id": "req-1",
            "status": "success",
            "elapsed_ms": 1,
            "exit_code": 0,
            "output_truncated": False,
        }
        runner = FakeRunner(stdout=json.dumps(frame))
        sb = _make_sandbox(runner=runner)
        result = sb.commands.run("ls")
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")

    def test_protocol_version_constant(self) -> None:
        self.assertEqual(PROTOCOL_VERSION, 1)


if __name__ == "__main__":
    unittest.main()
