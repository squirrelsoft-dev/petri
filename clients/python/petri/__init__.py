"""Petri sandbox Python client.

Re-exports the full public surface so callers can do::

    from petri import Sandbox, Commands, CommandResult, SandboxInfo
    from petri import PetriError, SandboxNotFoundError, PolicyDeniedError
    from petri import PROTOCOL_VERSION
"""

from petri.errors import (
    AuthorizationError,
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
from petri.sandbox import (
    PROTOCOL_VERSION,
    CommandOptions,
    CommandResult,
    Commands,
    ErrorFrame,
    ListOptions,
    Sandbox,
    SandboxCreateOptions,
    SandboxInfo,
)

__all__ = [
    # Core
    "Sandbox",
    "Commands",
    "CommandResult",
    "SandboxInfo",
    "ErrorFrame",
    # Options
    "SandboxCreateOptions",
    "CommandOptions",
    "ListOptions",
    # Errors
    "PetriError",
    "SandboxNotFoundError",
    "SandboxNotReadyError",
    "PolicyDeniedError",
    "CommandTimeoutError",
    "CommandFailedError",
    "OutputTruncatedError",
    "AuthorizationError",
    "ProtocolVersionMismatchError",
    "NotImplementedInV1Error",
    # Constants
    "PROTOCOL_VERSION",
]
