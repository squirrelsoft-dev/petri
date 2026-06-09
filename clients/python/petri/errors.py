"""Typed error hierarchy for the Petri SDK.

All errors extend PetriError. Transport/lifecycle errors are raised directly
by SDK methods. Policy/timeout/truncation errors are raised only when the
caller invokes result.raise_for_status().
"""

from __future__ import annotations


class PetriError(Exception):
    """Base class for all Petri SDK errors."""


class SandboxNotFoundError(PetriError):
    """Raised when the CLI stderr contains 'no sandbox with id'.

    The sandbox does not exist in the backend's known instance list.
    """


class SandboxNotReadyError(PetriError):
    """Raised when the CLI stderr contains 'not running'.

    The sandbox exists but is not in a running/ready state.
    """


class PolicyDeniedError(PetriError):
    """Raised by result.raise_for_status() when status == 'rejected'
    or error.code == 'policy_denied'.
    """


class CommandTimeoutError(PetriError):
    """Raised by result.raise_for_status() when status == 'timeout'."""


class CommandFailedError(PetriError):
    """Raised by result.raise_for_status() when status == 'failure'
    (non-zero exit).
    """

    def __init__(self, message: str, exit_code: int | None = None) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class OutputTruncatedError(PetriError):
    """Raised by result.raise_for_status() when output_truncated == True."""


class AuthorizationError(PetriError):
    """Raised when error.code is an auth/capability code (e.g.
    'capability_denied'). Reserved for future use.
    """


class ProtocolVersionMismatchError(PetriError):
    """Raised when the ResultFrame's protocol_version does not equal 1."""

    def __init__(self, actual: int) -> None:
        super().__init__(
            f"Protocol version mismatch: expected 1, got {actual}. "
            "Update the petri-sandbox client to match the petri binary."
        )
        self.actual = actual


class NotImplementedInV1Error(PetriError):
    """Raised when a reserved module (files, git, pty) is accessed in v1."""

    def __init__(self, module: str) -> None:
        super().__init__(
            f"The '{module}' module is not implemented in v1 of the Petri SDK."
        )
        self.module = module
