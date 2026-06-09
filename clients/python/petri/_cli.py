"""Runner abstraction: subprocess-based CLI invocation for the Petri binary.

A Runner is any callable that accepts a list of string argv, optional stdin
bytes, and returns a (stdout, stderr, returncode) triple. The default
implementation resolves the binary via:

  1. explicit ``petri_path`` argument
  2. ``PETRI_BIN`` environment variable
  3. ``"petri"`` on PATH
"""

from __future__ import annotations

import os
import subprocess
from typing import Protocol

from petri.errors import (
    PetriError,
    SandboxNotFoundError,
    SandboxNotReadyError,
)


class Runner(Protocol):
    """Protocol for an injectable CLI runner.

    Accepts the full argv (including the binary name as argv[0]), optional
    stdin bytes, and returns ``(stdout, stderr, returncode)``.
    """

    def __call__(
        self,
        argv: list[str],
        stdin: bytes | None = None,
    ) -> tuple[str, str, int]: ...


def _resolve_binary(petri_path: str | None = None) -> str:
    """Resolve the petri binary path.

    Priority: explicit *petri_path* → ``PETRI_BIN`` env var → ``"petri"``.
    """
    if petri_path:
        return petri_path
    env_path = os.environ.get("PETRI_BIN")
    if env_path:
        return env_path
    return "petri"


def make_default_runner(petri_path: str | None = None) -> Runner:
    """Return a subprocess-based Runner that resolves the binary automatically."""
    binary = _resolve_binary(petri_path)

    def _run(argv: list[str], stdin: bytes | None = None) -> tuple[str, str, int]:
        full_argv = [binary] + argv
        result = subprocess.run(
            full_argv,
            input=stdin,
            capture_output=True,
        )
        stdout = result.stdout.decode("utf-8", errors="replace")
        stderr = result.stderr.decode("utf-8", errors="replace")
        return stdout, stderr, result.returncode

    return _run


def _strip_prefix(message: str) -> str:
    """Strip the leading 'petri: ' prefix from a CLI error message."""
    prefix = "petri: "
    if message.startswith(prefix):
        return message[len(prefix):]
    return message


def check_cli_result(
    stdout: str,
    stderr: str,
    returncode: int,
) -> None:
    """Raise a typed error if *returncode* is non-zero.

    Maps well-known stderr patterns to specific error subclasses; anything else
    becomes the base :class:`~petri.errors.PetriError`.

    Args:
        stdout: The CLI's stdout (unused here; callers parse it themselves).
        stderr: The CLI's stderr output.
        returncode: The CLI process exit code.

    Raises:
        SandboxNotFoundError: stderr contains "no sandbox with id".
        SandboxNotReadyError: stderr contains "not running".
        PetriError: any other non-zero exit.
    """
    if returncode == 0:
        return

    # Normalise the error message: strip trailing whitespace, then the prefix.
    message = _strip_prefix(stderr.strip())

    if "no sandbox with id" in stderr:
        raise SandboxNotFoundError(message)
    if "not running" in stderr:
        raise SandboxNotReadyError(message)

    raise PetriError(message or f"petri exited with code {returncode}")
