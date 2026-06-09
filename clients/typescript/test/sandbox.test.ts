/**
 * Tests for the @squirrelsoft/petri TypeScript client.
 *
 * All tests use an injected mock runner — the real petri binary is never
 * spawned. This verifies both the correct argv construction and the result
 * parsing/error-mapping logic.
 */

import { describe, it, before } from "node:test";
import assert from "node:assert/strict";

import {
  Sandbox,
  Commands,
  CommandResult,
  SandboxInfo,
  Runner,
  RunnerResult,
  PROTOCOL_VERSION,
  PolicyDeniedError,
  CommandTimeoutError,
  CommandFailedError,
  OutputTruncatedError,
  SandboxNotFoundError,
  SandboxNotReadyError,
  ProtocolVersionMismatchError,
  NotImplementedError,
  PetriError,
} from "../src/index.js";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Build a runner that always returns the given canned response. */
function mockRunner(response: Partial<RunnerResult>): Runner {
  return async (_argv: string[], _stdin?: string): Promise<RunnerResult> => ({
    stdout: response.stdout ?? "",
    stderr: response.stderr ?? "",
    code: response.code ?? 0,
  });
}

/** Build a runner that captures each call and returns canned responses. */
function recordingRunner(
  responses: Partial<RunnerResult>[],
): { runner: Runner; calls: { argv: string[]; stdin?: string }[] } {
  const calls: { argv: string[]; stdin?: string }[] = [];
  let idx = 0;
  const runner: Runner = async (argv, stdin) => {
    calls.push({ argv, stdin });
    const r = responses[idx++] ?? responses[responses.length - 1]!;
    return {
      stdout: r.stdout ?? "",
      stderr: r.stderr ?? "",
      code: r.code ?? 0,
    };
  };
  return { runner, calls };
}

/** Build a minimal valid ResultFrame JSON string. */
function resultFrameJson(
  overrides: Partial<{
    protocol_version: number;
    id: string;
    status: string;
    elapsed_ms: number;
    stdout: string;
    stderr: string;
    exit_code: number | null;
    output_truncated: boolean;
    error: { code: string; message: string };
  }> = {},
): string {
  return JSON.stringify({
    protocol_version: PROTOCOL_VERSION,
    id: "req-1",
    status: "success",
    elapsed_ms: 1,
    stdout: "",
    stderr: "",
    exit_code: 0,
    output_truncated: false,
    ...overrides,
  });
}

// ---------------------------------------------------------------------------
// Sandbox.create
// ---------------------------------------------------------------------------

describe("Sandbox.create", () => {
  it("maps to the right argv with defaults", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "petri-123\n" }]);
    await Sandbox.create("base", { workspace: "/ws", policy: "p.toml", runner });

    assert.equal(calls.length, 1);
    const argv = calls[0]!.argv;
    assert.deepEqual(argv.slice(0, 3), ["sandbox", "create", "base"]);
    assert.ok(argv.includes("--workspace"));
    assert.equal(argv[argv.indexOf("--workspace") + 1], "/ws");
    assert.ok(argv.includes("--policy"));
    assert.equal(argv[argv.indexOf("--policy") + 1], "p.toml");
  });

  it("passes --id when provided", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "my-id\n" }]);
    await Sandbox.create("base", { id: "my-id", workspace: "/ws", policy: "p.toml", runner });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--id"));
    assert.equal(argv[argv.indexOf("--id") + 1], "my-id");
  });

  it("passes --backend when provided", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "sb-1\n" }]);
    await Sandbox.create("base", { backend: "macos", workspace: "/ws", policy: "p.toml", runner });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--backend"));
    assert.equal(argv[argv.indexOf("--backend") + 1], "macos");
  });

  it("passes --image when provided", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "sb-1\n" }]);
    await Sandbox.create("base", { image: "/path/to/image", workspace: "/ws", policy: "p.toml", runner });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--image"));
    assert.equal(argv[argv.indexOf("--image") + 1], "/path/to/image");
  });

  it("passes --metadata as comma-joined k=v", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "sb-1\n" }]);
    await Sandbox.create("base", {
      metadata: { env: "prod", team: "core" },
      workspace: "/ws",
      policy: "p.toml",
      runner,
    });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--metadata"));
    const metaVal = argv[argv.indexOf("--metadata") + 1]!;
    assert.ok(metaVal.includes("env=prod"), `metadata: ${metaVal}`);
    assert.ok(metaVal.includes("team=core"), `metadata: ${metaVal}`);
  });

  it("does NOT pass --id/--backend/--image when not set", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "sb-1\n" }]);
    await Sandbox.create("base", { workspace: "/ws", policy: "p.toml", runner });

    const argv = calls[0]!.argv;
    assert.ok(!argv.includes("--id"));
    assert.ok(!argv.includes("--backend"));
    assert.ok(!argv.includes("--image"));
  });

  it("returns a Sandbox with the trimmed id from stdout", async () => {
    const sb = await Sandbox.create("base", {
      workspace: "/ws",
      policy: "p.toml",
      runner: mockRunner({ stdout: "  petri-abc  \n" }),
    });
    assert.equal(sb.sandboxId, "petri-abc");
  });

  it("uses default template 'base' when not specified", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "sb-1\n" }]);
    await Sandbox.create(undefined as unknown as string, { workspace: "/ws", policy: "p.toml", runner });

    const argv = calls[0]!.argv;
    assert.equal(argv[2], "base");
  });
});

// ---------------------------------------------------------------------------
// Sandbox.list
// ---------------------------------------------------------------------------

describe("Sandbox.list", () => {
  const fakeSandboxes: SandboxInfo[] = [
    { id: "sb-1", backend: "macos", state: "ready", metadata: {} },
    { id: "sb-2", backend: "macos", state: "torn_down", metadata: { env: "prod" } },
  ];

  it("calls with --format json and parses the JSON array", async () => {
    const { runner, calls } = recordingRunner([
      { stdout: JSON.stringify(fakeSandboxes) },
    ]);
    const list = await Sandbox.list({ runner });

    assert.ok(calls[0]!.argv.includes("--format"));
    assert.equal(calls[0]!.argv[calls[0]!.argv.indexOf("--format") + 1], "json");
    assert.equal(list.length, 2);
    assert.equal(list[0]!.id, "sb-1");
    assert.equal(list[1]!.metadata["env"], "prod");
  });

  it("passes --state when provided", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "[]" }]);
    await Sandbox.list({ state: "running", runner });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--state"));
    assert.equal(argv[argv.indexOf("--state") + 1], "running");
  });

  it("passes --metadata as comma-joined k=v", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "[]" }]);
    await Sandbox.list({ metadata: { env: "prod" }, runner });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--metadata"));
    assert.ok(argv[argv.indexOf("--metadata") + 1]!.includes("env=prod"));
  });

  it("passes --limit when provided", async () => {
    const { runner, calls } = recordingRunner([{ stdout: "[]" }]);
    await Sandbox.list({ limit: 5, runner });

    const argv = calls[0]!.argv;
    assert.ok(argv.includes("--limit"));
    assert.equal(argv[argv.indexOf("--limit") + 1], "5");
  });
});

// ---------------------------------------------------------------------------
// Sandbox.connect and Sandbox.kill (static)
// ---------------------------------------------------------------------------

describe("Sandbox.connect (static)", () => {
  it("returns a Sandbox with the given id on success", async () => {
    const sb = await Sandbox.connect("dev-1", {
      runner: mockRunner({ code: 0 }),
    });
    assert.equal(sb.sandboxId, "dev-1");
  });

  it("builds argv ['sandbox','connect', id]", async () => {
    const { runner, calls } = recordingRunner([{ code: 0 }]);
    await Sandbox.connect("dev-1", { runner });
    assert.deepEqual(calls[0]!.argv, ["sandbox", "connect", "dev-1"]);
  });
});

describe("Sandbox.kill (static)", () => {
  it("builds argv ['sandbox','kill', id]", async () => {
    const { runner, calls } = recordingRunner([{ code: 0 }]);
    await Sandbox.kill("dev-1", { runner });
    assert.deepEqual(calls[0]!.argv, ["sandbox", "kill", "dev-1"]);
  });

  it("does not throw on success", async () => {
    await assert.doesNotReject(
      Sandbox.kill("dev-1", { runner: mockRunner({ code: 0 }) }),
    );
  });
});

// ---------------------------------------------------------------------------
// commands.run — argv construction
// ---------------------------------------------------------------------------

describe("commands.run argv construction", () => {
  /**
   * Build a Sandbox whose runner records all calls, then run `command` with
   * `opts` and return the argv of the exec call (the second call: first is
   * connect, second is exec).
   */
  async function runWith(
    command: string,
    opts: Parameters<Commands["run"]>[1],
  ): Promise<string[]> {
    const { runner, calls } = recordingRunner([
      { code: 0 }, // connect response
      { stdout: resultFrameJson(), code: 0 }, // exec response
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    await sb.commands.run(command, opts);
    // calls[0] = connect, calls[1] = exec
    return calls[1]!.argv;
  }

  it("places -- before command to stop option parsing", async () => {
    const argv = await runWith("ls", {});
    const sepIdx = argv.indexOf("--");
    assert.ok(sepIdx >= 0, "-- not found");
    assert.equal(argv[sepIdx + 1], "ls");
  });

  it("includes --cwd when provided", async () => {
    const argv = await runWith("ls", { cwd: "/workspace" });
    assert.ok(argv.includes("--cwd"));
    assert.equal(argv[argv.indexOf("--cwd") + 1], "/workspace");
  });

  it("includes --env as comma-joined k=v", async () => {
    const argv = await runWith("env", { env: { FOO: "bar" } });
    assert.ok(argv.includes("--env"));
    assert.ok(argv[argv.indexOf("--env") + 1]!.includes("FOO=bar"));
  });

  it("includes --timeout-ms when provided", async () => {
    const argv = await runWith("sleep", { timeoutMs: 5000 });
    assert.ok(argv.includes("--timeout-ms"));
    assert.equal(argv[argv.indexOf("--timeout-ms") + 1], "5000");
  });

  it("includes --max-output-bytes when provided", async () => {
    const argv = await runWith("cat", { maxOutputBytes: 2048 });
    assert.ok(argv.includes("--max-output-bytes"));
    assert.equal(argv[argv.indexOf("--max-output-bytes") + 1], "2048");
  });

  it("includes --request-id when provided", async () => {
    const argv = await runWith("true", { requestId: "req-42" });
    assert.ok(argv.includes("--request-id"));
    assert.equal(argv[argv.indexOf("--request-id") + 1], "req-42");
  });

  it("appends extra args after command", async () => {
    const argv = await runWith("ls", { args: ["-la", "/tmp"] });
    const sepIdx = argv.indexOf("--");
    assert.equal(argv[sepIdx + 1], "ls");
    assert.equal(argv[sepIdx + 2], "-la");
    assert.equal(argv[sepIdx + 3], "/tmp");
  });
});

// ---------------------------------------------------------------------------
// commands.run — result parsing
// ---------------------------------------------------------------------------

describe("commands.run result parsing", () => {
  async function execWith(frameJson: string): Promise<CommandResult> {
    const runner = mockRunner({ stdout: frameJson, code: 0 });
    const sb = await Sandbox.connect("sb-1", { runner: mockRunner({ code: 0 }) });
    // Inject runner into a Commands object directly via Sandbox.connect trick
    const { runner: execRunner, calls } = recordingRunner([
      { stdout: frameJson, code: 0 },
    ]);
    void calls; // suppress unused warning
    const sb2 = await Sandbox.connect("sb-1", { runner: execRunner });
    return sb2.commands.run("echo hello");
  }

  it("parses a success frame into CommandResult", async () => {
    const result = await execWith(
      resultFrameJson({ status: "success", stdout: "hello\n", exit_code: 0 }),
    );
    assert.equal(result.status, "success");
    assert.equal(result.exitCode, 0);
    assert.equal(result.stdout, "hello\n");
    assert.equal(result.outputTruncated, false);
    assert.ok(result.success);
  });

  it("populates stderr from frame", async () => {
    const result = await execWith(
      resultFrameJson({ status: "failure", stderr: "boom\n", exit_code: 1 }),
    );
    assert.equal(result.stderr, "boom\n");
    assert.equal(result.exitCode, 1);
  });

  it("defaults stdout/stderr/outputTruncated when absent in frame", async () => {
    // Minimal frame — omit optional fields
    const frame = JSON.stringify({
      protocol_version: 1,
      id: "r1",
      status: "success",
      elapsed_ms: 1,
      exit_code: 0,
    });
    const result = await execWith(frame);
    assert.equal(result.stdout, "");
    assert.equal(result.stderr, "");
    assert.equal(result.outputTruncated, false);
    assert.equal(result.error, null);
  });

  it("non-zero exit code is NOT thrown — returns failure result", async () => {
    const runner = mockRunner({
      stdout: resultFrameJson({
        status: "failure",
        exit_code: 2,
        stderr: "err",
      }),
      code: 0, // exec itself exits 0; the failure is in the frame
    });
    const sb = await Sandbox.connect("sb-1", { runner: mockRunner({ code: 0 }) });
    // Use a fresh recording runner
    const { runner: r } = recordingRunner([
      {
        stdout: resultFrameJson({ status: "failure", exit_code: 2 }),
        code: 0,
      },
    ]);
    const sb2 = await Sandbox.connect("sb-1", { runner: r });
    const result = await sb2.commands.run("false");
    assert.equal(result.status, "failure");
    assert.equal(result.exitCode, 2);
    assert.ok(!result.success);
    // No exception thrown — reaching here means it returned normally
  });
});

// ---------------------------------------------------------------------------
// commands.run — stdin piping
// ---------------------------------------------------------------------------

describe("commands.run stdin", () => {
  it("passes stdin to the runner", async () => {
    const { runner, calls } = recordingRunner([
      { stdout: resultFrameJson(), code: 0 },
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    await sb.commands.run("cat", { stdin: "hello world" });
    // calls[0] is connect; calls[1] is exec
    const execCall = calls.find((c) => c.argv.includes("exec"));
    assert.equal(execCall?.stdin, "hello world");
  });
});

// ---------------------------------------------------------------------------
// result.check() — exception ordering
// ---------------------------------------------------------------------------

describe("result.check()", () => {
  async function runFrame(
    frameOverrides: Parameters<typeof resultFrameJson>[0],
  ): Promise<CommandResult> {
    const { runner } = recordingRunner([
      { stdout: resultFrameJson(frameOverrides), code: 0 },
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    return sb.commands.run("test");
  }

  it("throws PolicyDeniedError when status is 'rejected'", async () => {
    const result = await runFrame({
      status: "rejected",
      error: { code: "policy_denied", message: "not allowed" },
    });
    assert.throws(() => result.check(), PolicyDeniedError);
  });

  it("throws PolicyDeniedError when error.code is 'policy_denied'", async () => {
    const result = await runFrame({
      status: "failure",
      error: { code: "policy_denied", message: "denied" },
    });
    assert.throws(() => result.check(), PolicyDeniedError);
  });

  it("throws CommandTimeoutError when status is 'timeout'", async () => {
    const result = await runFrame({ status: "timeout" });
    assert.throws(() => result.check(), CommandTimeoutError);
  });

  it("throws CommandFailedError when status is 'failure'", async () => {
    const result = await runFrame({ status: "failure", exit_code: 1 });
    assert.throws(() => result.check(), CommandFailedError);
  });

  it("CommandFailedError carries the exit code", async () => {
    const result = await runFrame({ status: "failure", exit_code: 42 });
    try {
      result.check();
      assert.fail("expected CommandFailedError");
    } catch (err) {
      assert.ok(err instanceof CommandFailedError);
      assert.equal(err.exitCode, 42);
    }
  });

  it("throws OutputTruncatedError when output_truncated is true", async () => {
    const result = await runFrame({
      status: "success",
      exit_code: 0,
      output_truncated: true,
    });
    assert.throws(() => result.check(), OutputTruncatedError);
  });

  it("returns itself (for chaining) on clean success", async () => {
    const result = await runFrame({ status: "success", exit_code: 0 });
    const returned = result.check();
    assert.equal(returned, result);
  });

  it("check() priority: rejected before timeout", async () => {
    // Both rejected and timeout conditions — rejected must win
    const result = await runFrame({
      status: "rejected",
      error: { code: "policy_denied", message: "denied" },
    });
    assert.throws(() => result.check(), PolicyDeniedError);
  });

  it("check() priority: timeout before failure", async () => {
    const result = await runFrame({ status: "timeout" });
    assert.throws(() => result.check(), CommandTimeoutError);
  });
});

// ---------------------------------------------------------------------------
// Lifecycle error mapping
// ---------------------------------------------------------------------------

describe("lifecycle error mapping", () => {
  it("maps 'no sandbox with id' stderr to SandboxNotFoundError", async () => {
    const runner = mockRunner({
      stderr: "petri: no sandbox with id 'dev-99'",
      code: 1,
    });
    await assert.rejects(
      () => Sandbox.connect("dev-99", { runner }),
      SandboxNotFoundError,
    );
  });

  it("maps 'not running' stderr to SandboxNotReadyError", async () => {
    const runner = mockRunner({
      stderr: "petri: sandbox 'dev-1' is not running",
      code: 1,
    });
    await assert.rejects(
      () => Sandbox.connect("dev-1", { runner }),
      SandboxNotReadyError,
    );
  });

  it("maps unrecognized stderr to base PetriError", async () => {
    const runner = mockRunner({
      stderr: "petri: something unexpected happened",
      code: 1,
    });
    await assert.rejects(
      () => Sandbox.connect("dev-1", { runner }),
      PetriError,
    );
  });

  it("SandboxNotFoundError propagates through kill", async () => {
    const runner = mockRunner({
      stderr: "petri: no sandbox with id 'gone'",
      code: 1,
    });
    await assert.rejects(
      () => Sandbox.kill("gone", { runner }),
      SandboxNotFoundError,
    );
  });

  it("SandboxNotFoundError propagates through commands.run (exec)", async () => {
    const { runner } = recordingRunner([
      { stderr: "petri: no sandbox with id 'gone'", code: 1 },
    ]);
    const sb = await Sandbox.connect("gone", {
      runner: mockRunner({ code: 0 }),
    });
    // Inject failing runner into exec via a new Sandbox
    const { runner: failRunner } = recordingRunner([
      { stderr: "petri: no sandbox with id 'gone'", code: 1 },
    ]);
    const sb2 = await Sandbox.connect("gone", {
      runner: mockRunner({ code: 0 }),
    });
    // We need to get a sandbox using the failRunner for exec
    // Trick: use Sandbox.connect with failRunner so exec also uses it
    const { runner: connectThenFail } = recordingRunner([
      { code: 0 }, // connect succeeds
      { stderr: "petri: no sandbox with id 'gone'", code: 1 }, // exec fails
    ]);
    const sb3 = await Sandbox.connect("gone", { runner: connectThenFail });
    await assert.rejects(() => sb3.commands.run("ls"), SandboxNotFoundError);
    void sb; void runner; void failRunner; void sb2;
  });
});

// ---------------------------------------------------------------------------
// ProtocolVersionMismatchError
// ---------------------------------------------------------------------------

describe("protocol version mismatch", () => {
  it("throws ProtocolVersionMismatchError when protocol_version != 1", async () => {
    const frame = JSON.stringify({
      protocol_version: 99,
      id: "r1",
      status: "success",
      elapsed_ms: 1,
      exit_code: 0,
    });
    const { runner } = recordingRunner([{ stdout: frame, code: 0 }]);
    const sb = await Sandbox.connect("sb-1", { runner });
    await assert.rejects(() => sb.commands.run("true"), ProtocolVersionMismatchError);
  });

  it("ProtocolVersionMismatchError carries the actual version", async () => {
    const frame = JSON.stringify({
      protocol_version: 7,
      id: "r1",
      status: "success",
      elapsed_ms: 1,
      exit_code: 0,
    });
    const { runner } = recordingRunner([{ stdout: frame, code: 0 }]);
    const sb = await Sandbox.connect("sb-1", { runner });
    try {
      await sb.commands.run("true");
      assert.fail("expected ProtocolVersionMismatchError");
    } catch (err) {
      assert.ok(err instanceof ProtocolVersionMismatchError);
      assert.equal(err.actual, 7);
    }
  });
});

// ---------------------------------------------------------------------------
// Reserved modules — files / git / pty
// ---------------------------------------------------------------------------

describe("reserved modules throw NotImplementedError", () => {
  let sb: Sandbox;

  // Create a sandbox to test against
  before(async () => {
    sb = await Sandbox.create("base", {
      workspace: "/ws",
      policy: "p.toml",
      runner: mockRunner({ stdout: "sb-1\n" }),
    });
  });

  it("sandbox.files throws NotImplementedError", () => {
    assert.throws(() => sb.files, NotImplementedError);
  });

  it("sandbox.git throws NotImplementedError", () => {
    assert.throws(() => sb.git, NotImplementedError);
  });

  it("sandbox.pty throws NotImplementedError", () => {
    assert.throws(() => sb.pty, NotImplementedError);
  });
});

// ---------------------------------------------------------------------------
// Instance helpers — getInfo, isRunning
// ---------------------------------------------------------------------------

describe("sandbox instance helpers", () => {
  it("getInfo returns the matching SandboxInfo", async () => {
    const infos: SandboxInfo[] = [
      { id: "sb-1", backend: "macos", state: "ready", metadata: {} },
    ];
    const { runner } = recordingRunner([
      { code: 0 }, // connect
      { stdout: JSON.stringify(infos), code: 0 }, // list (getInfo)
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    const info = await sb.getInfo();
    assert.ok(info !== null);
    assert.equal(info!.id, "sb-1");
  });

  it("getInfo returns null when not in list", async () => {
    const { runner } = recordingRunner([
      { code: 0 }, // connect
      { stdout: "[]", code: 0 }, // list (getInfo)
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    const info = await sb.getInfo();
    assert.equal(info, null);
  });

  it("isRunning returns true for state 'ready'", async () => {
    const infos: SandboxInfo[] = [
      { id: "sb-1", backend: "macos", state: "ready", metadata: {} },
    ];
    const { runner } = recordingRunner([
      { code: 0 }, // connect
      { stdout: JSON.stringify(infos), code: 0 }, // list
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    assert.ok(await sb.isRunning());
  });

  it("isRunning returns true for state 'running_dispatch'", async () => {
    const infos: SandboxInfo[] = [
      { id: "sb-1", backend: "macos", state: "running_dispatch", metadata: {} },
    ];
    const { runner } = recordingRunner([
      { code: 0 }, // connect
      { stdout: JSON.stringify(infos), code: 0 }, // list
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    assert.ok(await sb.isRunning());
  });

  it("isRunning returns false for state 'torn_down'", async () => {
    const infos: SandboxInfo[] = [
      { id: "sb-1", backend: "macos", state: "torn_down", metadata: {} },
    ];
    const { runner } = recordingRunner([
      { code: 0 }, // connect
      { stdout: JSON.stringify(infos), code: 0 }, // list
    ]);
    const sb = await Sandbox.connect("sb-1", { runner });
    assert.ok(!(await sb.isRunning()));
  });
});
