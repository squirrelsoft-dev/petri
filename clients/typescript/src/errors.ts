/**
 * Typed error hierarchy for the Petri SDK.
 *
 * All errors extend PetriError. Transport/lifecycle errors are thrown directly
 * by SDK methods. Policy/timeout/truncation errors are thrown only when the
 * caller invokes result.check().
 */

/** Base class for all Petri SDK errors. */
export class PetriError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PetriError";
    // Maintain proper prototype chain in transpiled ES5 targets
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown when the CLI stderr contains "no sandbox with id".
 * The sandbox does not exist in the backend's known instance list.
 */
export class SandboxNotFoundError extends PetriError {
  constructor(message: string) {
    super(message);
    this.name = "SandboxNotFoundError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown when the CLI stderr contains "not running".
 * The sandbox exists but is not in a running/ready state.
 */
export class SandboxNotReadyError extends PetriError {
  constructor(message: string) {
    super(message);
    this.name = "SandboxNotReadyError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown by result.check() when status === "rejected" or error.code === "policy_denied".
 */
export class PolicyDeniedError extends PetriError {
  constructor(message: string) {
    super(message);
    this.name = "PolicyDeniedError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown by result.check() when status === "timeout".
 */
export class CommandTimeoutError extends PetriError {
  constructor(message: string) {
    super(message);
    this.name = "CommandTimeoutError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown by result.check() when status === "failure" (non-zero exit).
 */
export class CommandFailedError extends PetriError {
  /** The exit code, if available. */
  readonly exitCode: number | null;

  constructor(message: string, exitCode: number | null) {
    super(message);
    this.name = "CommandFailedError";
    this.exitCode = exitCode;
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown by result.check() when output_truncated === true.
 */
export class OutputTruncatedError extends PetriError {
  constructor(message: string) {
    super(message);
    this.name = "OutputTruncatedError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown by result.check() when error.code is an auth/capability code
 * (e.g., "capability_denied"). Reserved for future use.
 */
export class AuthorizationError extends PetriError {
  constructor(message: string) {
    super(message);
    this.name = "AuthorizationError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown when the ResultFrame's protocol_version does not equal 1.
 */
export class ProtocolVersionMismatchError extends PetriError {
  readonly actual: number;

  constructor(actual: number) {
    super(
      `Protocol version mismatch: expected 1, got ${actual}. ` +
        "Update the @squirrelsoft/petri client to match the petri binary.",
    );
    this.name = "ProtocolVersionMismatchError";
    this.actual = actual;
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/**
 * Thrown when a reserved module (files, git, pty) is accessed in v1.
 */
export class NotImplementedError extends PetriError {
  constructor(module: string) {
    super(`The '${module}' module is not implemented in v1 of the Petri SDK.`);
    this.name = "NotImplementedError";
    Object.setPrototypeOf(this, new.target.prototype);
  }
}
