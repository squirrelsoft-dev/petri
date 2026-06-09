/**
 * @squirrelsoft/petri — TypeScript client for the Petri sandbox.
 *
 * Transport: thin wrapper over the `petri` CLI. Every SDK call shells out to
 * `petri sandbox ...`, captures stdout/stderr/exit-code, and parses the CLI's
 * JSON output. An injectable Runner allows tests to exercise the full surface
 * without a real binary.
 *
 * v1 implements lifecycle (create / connect / list / kill / getInfo /
 * isRunning) and the commands module. The files, git, and pty modules are
 * named and reserved but throw NotImplementedError.
 */

export * from "./errors.js";

import {
  PetriError,
  SandboxNotFoundError,
  SandboxNotReadyError,
  ProtocolVersionMismatchError,
  PolicyDeniedError,
  CommandTimeoutError,
  CommandFailedError,
  OutputTruncatedError,
  AuthorizationError,
  NotImplementedError,
} from "./errors.js";

import { spawn } from "node:child_process";

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

/** The only ResultFrame protocol_version this client understands. */
export const PROTOCOL_VERSION = 1;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Raw wire shape of a ResultFrame as emitted by `petri sandbox exec`. */
interface ResultFrame {
  protocol_version: number;
  id: string;
  status: CommandStatus;
  elapsed_ms: number;
  stdout?: string;
  stderr?: string;
  exit_code?: number | null;
  output_truncated?: boolean;
  error?: ErrorFrame;
}

/** Structured error payload inside a ResultFrame. */
export interface ErrorFrame {
  code: string;
  message: string;
  details?: Record<string, unknown>;
}

/** Dispatch status values from the wire protocol. */
export type CommandStatus =
  | "success"
  | "failure"
  | "rejected"
  | "timeout"
  | "cancelled"
  | "malformed";

/** SDK-facing view of a dispatch ResultFrame. */
export interface CommandResult {
  /** Dispatch status. */
  status: CommandStatus;
  /** Process exit code (null when not available). */
  exitCode: number | null;
  /** Captured standard output (empty string when absent). */
  stdout: string;
  /** Captured standard error (empty string when absent). */
  stderr: string;
  /** Whether output was truncated against maxOutputBytes. */
  outputTruncated: boolean;
  /** Structured error frame for non-success statuses, or null. */
  error: ErrorFrame | null;
  /** True when status is "success" and exitCode is 0. */
  readonly success: boolean;
  /**
   * Assert success, throwing the first applicable error in this order:
   *   protocol mismatch → rejected/policy → timeout → command-failed → truncated
   *
   * Returns `this` for chaining on clean success.
   */
  check(): this;
}

/** Lifecycle info for a sandbox as returned by `Sandbox.list`. */
export interface SandboxInfo {
  id: string;
  backend: string;
  state: string;
  metadata: Record<string, string>;
}

// ---------------------------------------------------------------------------
// Runner type — injectable transport
// ---------------------------------------------------------------------------

/** Result from running the petri CLI. */
export interface RunnerResult {
  stdout: string;
  stderr: string;
  code: number | null;
}

/**
 * Async function that invokes the petri CLI with the given argv and optional
 * stdin. The default runner spawns the real binary; tests inject a mock.
 */
export type Runner = (
  argv: string[],
  stdin?: string,
) => Promise<RunnerResult>;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/** Options accepted by Sandbox.create. */
export interface SandboxOpts {
  /** Explicit sandbox id. Generated when omitted. */
  id?: string;
  /** Host workspace directory mounted into the sandbox. */
  workspace?: string;
  /** Policy file path applied at boot. */
  policy?: string;
  /** Backend name (defaults to "macos"). */
  backend?: string;
  /** Image bundle path. Uses the backend's default when omitted. */
  image?: string;
  /** Free-form metadata persisted with the instance. */
  metadata?: Record<string, string>;
  /** Override the petri binary path (also checked: PETRI_BIN env var, then "petri"). */
  petriPath?: string;
  /** Injectable CLI runner (used by tests to avoid spawning the real binary). */
  runner?: Runner;
}

/** Options shared by connect / kill / list that need a runner or petriPath. */
export interface SharedOpts {
  petriPath?: string;
  runner?: Runner;
}

/** Options for Sandbox.list. */
export interface ListOpts extends SharedOpts {
  /** Filter to sandboxes in this state. */
  state?: string;
  /** Filter to sandboxes matching all these metadata key=value pairs. */
  metadata?: Record<string, string>;
  /** Maximum number of results. */
  limit?: number;
}

/** Options for commands.run. */
export interface CommandOpts {
  /** Working directory inside the sandbox. */
  cwd?: string;
  /** Extra arguments appended after the command. */
  args?: string[];
  /** Environment overrides. */
  env?: Record<string, string>;
  /** Standard input piped to the process. */
  stdin?: string;
  /** Per-request wall-clock timeout in milliseconds. */
  timeoutMs?: number;
  /** Maximum captured output bytes before truncation. */
  maxOutputBytes?: number;
  /** Explicit request id for correlation. Generated when omitted. */
  requestId?: string;
}

// ---------------------------------------------------------------------------
// Default runner — spawns the real petri binary
// ---------------------------------------------------------------------------

function resolvePetriPath(petriPath?: string): string {
  return petriPath ?? process.env["PETRI_BIN"] ?? "petri";
}

function makeDefaultRunner(petriPath?: string): Runner {
  const bin = resolvePetriPath(petriPath);
  return (argv: string[], stdin?: string): Promise<RunnerResult> => {
    return new Promise((resolve, reject) => {
      const child = spawn(bin, argv, {
        stdio: ["pipe", "pipe", "pipe"],
      });

      const stdoutChunks: Buffer[] = [];
      const stderrChunks: Buffer[] = [];

      child.stdout.on("data", (chunk: Buffer) => stdoutChunks.push(chunk));
      child.stderr.on("data", (chunk: Buffer) => stderrChunks.push(chunk));

      child.on("error", (err) => {
        reject(
          new PetriError(
            `Failed to spawn petri binary '${bin}': ${err.message}`,
          ),
        );
      });

      child.on("close", (code) => {
        resolve({
          stdout: Buffer.concat(stdoutChunks).toString("utf8"),
          stderr: Buffer.concat(stderrChunks).toString("utf8"),
          code,
        });
      });

      if (stdin !== undefined) {
        child.stdin.write(stdin, "utf8");
      }
      child.stdin.end();
    });
  };
}

// ---------------------------------------------------------------------------
// Lifecycle error mapping (non-zero exit from non-exec commands)
// ---------------------------------------------------------------------------

function mapLifecycleError(stderr: string): PetriError {
  // Strip the leading "petri: " prefix the CLI adds
  const msg = stderr.replace(/^petri:\s*/m, "").trim();
  if (msg.includes("no sandbox with id")) {
    return new SandboxNotFoundError(msg);
  }
  if (msg.includes("not running")) {
    return new SandboxNotReadyError(msg);
  }
  return new PetriError(msg || stderr.trim());
}

// ---------------------------------------------------------------------------
// CommandResult implementation
// ---------------------------------------------------------------------------

function makeCommandResult(frame: ResultFrame): CommandResult {
  const result: CommandResult = {
    status: frame.status,
    exitCode: frame.exit_code ?? null,
    stdout: frame.stdout ?? "",
    stderr: frame.stderr ?? "",
    outputTruncated: frame.output_truncated ?? false,
    error: frame.error ?? null,
    get success(): boolean {
      return result.status === "success" && result.exitCode === 0;
    },
    check(): typeof result {
      // Order per contract: rejected/policy → timeout → command-failed → truncated
      if (
        result.status === "rejected" ||
        result.error?.code === "policy_denied"
      ) {
        throw new PolicyDeniedError(
          result.error?.message ?? "Command rejected by policy",
        );
      }
      if (result.status === "timeout") {
        throw new CommandTimeoutError(
          result.error?.message ?? "Command timed out",
        );
      }
      if (result.status === "failure") {
        throw new CommandFailedError(
          result.error?.message ??
            `Command failed with exit code ${result.exitCode}`,
          result.exitCode,
        );
      }
      if (result.outputTruncated) {
        throw new OutputTruncatedError(
          "Command output was truncated (maxOutputBytes exceeded)",
        );
      }
      // Check for authorization errors
      if (result.error?.code === "capability_denied") {
        throw new AuthorizationError(
          result.error.message ?? "Authorization denied",
        );
      }
      return result;
    },
  };
  return result;
}

// ---------------------------------------------------------------------------
// Commands module
// ---------------------------------------------------------------------------

/** The commands module: run shell commands inside a sandbox. */
export class Commands {
  readonly #sandboxId: string;
  readonly #runner: Runner;

  constructor(sandboxId: string, runner: Runner) {
    this.#sandboxId = sandboxId;
    this.#runner = runner;
  }

  /**
   * Run a command inside the sandbox and return a typed CommandResult.
   *
   * Does NOT throw for non-success status or non-zero exit (those are normal
   * results). Throws only on transport/usage failures or protocol version
   * mismatch. Call result.check() to opt in to exceptions.
   */
  async run(command: string, opts: CommandOpts = {}): Promise<CommandResult> {
    const argv: string[] = ["sandbox", "exec", this.#sandboxId];

    if (opts.cwd) {
      argv.push("--cwd", opts.cwd);
    }
    if (opts.env && Object.keys(opts.env).length > 0) {
      const pairs = Object.entries(opts.env)
        .map(([k, v]) => `${k}=${v}`)
        .join(",");
      argv.push("--env", pairs);
    }
    if (opts.timeoutMs !== undefined) {
      argv.push("--timeout-ms", String(opts.timeoutMs));
    }
    if (opts.maxOutputBytes !== undefined) {
      argv.push("--max-output-bytes", String(opts.maxOutputBytes));
    }
    if (opts.requestId) {
      argv.push("--request-id", opts.requestId);
    }

    // Stop option parsing before command; prevents leading-dash commands from
    // being misread as flags.
    argv.push("--", command);

    if (opts.args && opts.args.length > 0) {
      argv.push(...opts.args);
    }

    const result = await this.#runner(argv, opts.stdin);

    if (result.code !== 0) {
      // Non-zero from exec means transport/lifecycle failure, not command failure
      throw mapLifecycleError(result.stderr);
    }

    let frame: ResultFrame;
    try {
      frame = JSON.parse(result.stdout) as ResultFrame;
    } catch {
      throw new PetriError(
        `Malformed JSON from petri sandbox exec: ${result.stdout}`,
      );
    }

    if (frame.protocol_version !== PROTOCOL_VERSION) {
      throw new ProtocolVersionMismatchError(frame.protocol_version);
    }

    return makeCommandResult(frame);
  }
}

// ---------------------------------------------------------------------------
// Reserved module stubs
// ---------------------------------------------------------------------------

/** Reserved stub — not implemented in v1. */
class ReservedModule {
  readonly #name: string;
  constructor(name: string) {
    this.#name = name;
  }
  /** @throws NotImplementedError always. */
  _notImplemented(): never {
    throw new NotImplementedError(this.#name);
  }
}

/** Reserved Filesystem module (not implemented in v1). */
export class Filesystem extends ReservedModule {
  constructor() {
    super("files");
  }
}

/** Reserved Git module (not implemented in v1). */
export class Git extends ReservedModule {
  constructor() {
    super("git");
  }
}

/** Reserved Pty module (not implemented in v1). */
export class Pty extends ReservedModule {
  constructor() {
    super("pty");
  }
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/** A handle to a single Petri sandbox. */
export class Sandbox {
  readonly #sandboxId: string;
  readonly #runner: Runner;
  readonly #commands: Commands;
  readonly #files: Filesystem;
  readonly #git: Git;
  readonly #pty: Pty;

  private constructor(sandboxId: string, runner: Runner) {
    this.#sandboxId = sandboxId;
    this.#runner = runner;
    this.#commands = new Commands(sandboxId, runner);
    this.#files = new Filesystem();
    this.#git = new Git();
    this.#pty = new Pty();
  }

  // -------------------------------------------------------------------------
  // Static factory methods
  // -------------------------------------------------------------------------

  /**
   * Create a new sandbox and return a handle to it.
   *
   * @param template  Image template name (defaults to "base").
   * @param opts      Creation options.
   */
  static async create(
    template: string = "base",
    opts: SandboxOpts = {},
  ): Promise<Sandbox> {
    const runner =
      opts.runner ?? makeDefaultRunner(opts.petriPath);

    const argv: string[] = [
      "sandbox",
      "create",
      template,
      "--workspace",
      opts.workspace ?? ".",
      "--policy",
      opts.policy ?? "policy.toml",
    ];

    if (opts.id) {
      argv.push("--id", opts.id);
    }
    if (opts.backend) {
      argv.push("--backend", opts.backend);
    }
    if (opts.image) {
      argv.push("--image", opts.image);
    }
    if (opts.metadata && Object.keys(opts.metadata).length > 0) {
      const pairs = Object.entries(opts.metadata)
        .map(([k, v]) => `${k}=${v}`)
        .join(",");
      argv.push("--metadata", pairs);
    }

    const result = await runner(argv);

    if (result.code !== 0) {
      throw mapLifecycleError(result.stderr);
    }

    const sandboxId = result.stdout.trim();
    if (!sandboxId) {
      throw new PetriError("petri sandbox create returned an empty sandbox id");
    }

    return new Sandbox(sandboxId, runner);
  }

  /**
   * Connect to an existing running sandbox.
   *
   * Throws SandboxNotFoundError or SandboxNotReadyError if the sandbox does
   * not exist or is not running. Never tears the sandbox down.
   */
  static async connect(
    sandboxId: string,
    opts: SharedOpts = {},
  ): Promise<Sandbox> {
    const runner = opts.runner ?? makeDefaultRunner(opts.petriPath);
    const result = await runner(["sandbox", "connect", sandboxId]);

    if (result.code !== 0) {
      throw mapLifecycleError(result.stderr);
    }

    return new Sandbox(sandboxId, runner);
  }

  /**
   * List all sandboxes known to the backend.
   */
  static async list(opts: ListOpts = {}): Promise<SandboxInfo[]> {
    const runner = opts.runner ?? makeDefaultRunner(opts.petriPath);

    const argv: string[] = ["sandbox", "list", "--format", "json"];

    if (opts.state) {
      argv.push("--state", opts.state);
    }
    if (opts.metadata && Object.keys(opts.metadata).length > 0) {
      const pairs = Object.entries(opts.metadata)
        .map(([k, v]) => `${k}=${v}`)
        .join(",");
      argv.push("--metadata", pairs);
    }
    if (opts.limit !== undefined) {
      argv.push("--limit", String(opts.limit));
    }

    const result = await runner(argv);

    if (result.code !== 0) {
      throw mapLifecycleError(result.stderr);
    }

    let parsed: unknown;
    try {
      parsed = JSON.parse(result.stdout);
    } catch {
      throw new PetriError(
        `Malformed JSON from petri sandbox list: ${result.stdout}`,
      );
    }

    if (!Array.isArray(parsed)) {
      throw new PetriError(
        `Expected JSON array from petri sandbox list, got: ${typeof parsed}`,
      );
    }

    return parsed as SandboxInfo[];
  }

  /**
   * Kill a sandbox by id without holding a handle to it.
   */
  static async kill(sandboxId: string, opts: SharedOpts = {}): Promise<void> {
    const runner = opts.runner ?? makeDefaultRunner(opts.petriPath);
    const result = await runner(["sandbox", "kill", sandboxId]);

    if (result.code !== 0) {
      throw mapLifecycleError(result.stderr);
    }
  }

  /**
   * Get info for a single sandbox by id, or null if not found.
   */
  static async getInfo(
    sandboxId: string,
    opts: SharedOpts = {},
  ): Promise<SandboxInfo | null> {
    const all = await Sandbox.list(opts);
    return all.find((s) => s.id === sandboxId) ?? null;
  }

  // -------------------------------------------------------------------------
  // Instance properties
  // -------------------------------------------------------------------------

  /** The sandbox id. */
  get sandboxId(): string {
    return this.#sandboxId;
  }

  /** The commands module for running shell commands inside the sandbox. */
  get commands(): Commands {
    return this.#commands;
  }

  /**
   * Reserved Filesystem module — not implemented in v1.
   * @throws NotImplementedError always when any operation is called.
   */
  get files(): Filesystem {
    throw new NotImplementedError("files");
  }

  /**
   * Reserved Git module — not implemented in v1.
   * @throws NotImplementedError always when any operation is called.
   */
  get git(): Git {
    throw new NotImplementedError("git");
  }

  /**
   * Reserved Pty module — not implemented in v1.
   * @throws NotImplementedError always when any operation is called.
   */
  get pty(): Pty {
    throw new NotImplementedError("pty");
  }

  // -------------------------------------------------------------------------
  // Instance methods
  // -------------------------------------------------------------------------

  /** Tear this sandbox down. */
  async kill(): Promise<void> {
    return Sandbox.kill(this.#sandboxId, { runner: this.#runner });
  }

  /** Get the current lifecycle info for this sandbox, or null if not found. */
  async getInfo(): Promise<SandboxInfo | null> {
    return Sandbox.getInfo(this.#sandboxId, { runner: this.#runner });
  }

  /** Returns true when the sandbox is in a running or ready state. */
  async isRunning(): Promise<boolean> {
    const info = await this.getInfo();
    if (!info) return false;
    return info.state === "ready" || info.state === "running_dispatch";
  }

  /** Re-attach to / refresh this sandbox (connects and checks it is running). */
  async connect(): Promise<void> {
    await Sandbox.connect(this.#sandboxId, { runner: this.#runner });
  }
}
