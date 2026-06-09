# @squirrelsoft/petri

TypeScript client for the [Petri](https://github.com/squirrelsoft/petri) sandbox.

Thin wrapper over the `petri` CLI. Every SDK call shells out to `petri sandbox …`, captures stdout/stderr/exit-code, and parses the CLI's JSON output. When the HTTP control plane lands, only the transport layer changes — the public `Sandbox` surface stays the same.

## Installation

```bash
npm install @squirrelsoft/petri
```

## Quick start

```ts
import { Sandbox } from "@squirrelsoft/petri"

// Create a sandbox
const sandbox = await Sandbox.create("base", {
  workspace: ".",
  policy: "./policy.toml",
})

// Run a command — never throws for non-zero exit
const result = await sandbox.commands.run("cargo test", { cwd: "/workspace" })

console.log(result.stdout)
console.log("exit code:", result.exitCode)
console.log("success:", result.success)

// Opt in to exceptions via check()
result.check()   // throws PolicyDeniedError / CommandTimeoutError / etc.

// Tear down
await sandbox.kill()
```

## Binary resolution

The client resolves the `petri` binary in this order:

1. `opts.petriPath`
2. `PETRI_BIN` environment variable
3. `petri` on `PATH`

## Sandbox lifecycle

```ts
// Connect to an existing sandbox
const sandbox = await Sandbox.connect("petri-abc123")

// List sandboxes
const sandboxes = await Sandbox.list({ state: "running" })

// Kill by id (static)
await Sandbox.kill("petri-abc123")

// Instance methods
await sandbox.kill()
const info = await sandbox.getInfo()    // SandboxInfo | null
const running = await sandbox.isRunning()
```

## Error types

| Error | When |
|---|---|
| `SandboxNotFoundError` | CLI stderr contains `no sandbox with id` |
| `SandboxNotReadyError` | CLI stderr contains `not running` |
| `PolicyDeniedError` | `result.check()` — status is `rejected` or `error.code === "policy_denied"` |
| `CommandTimeoutError` | `result.check()` — status is `timeout` |
| `CommandFailedError` | `result.check()` — status is `failure` (non-zero exit) |
| `OutputTruncatedError` | `result.check()` — `outputTruncated` is true |
| `ProtocolVersionMismatchError` | `frame.protocol_version !== 1` |
| `PetriError` | Base class; any other CLI failure |

## Testing with a mock runner

Inject a `runner` to avoid spawning the real binary in tests:

```ts
import { Sandbox, Runner } from "@squirrelsoft/petri"

const mockRunner: Runner = async (argv, stdin) => ({
  stdout: "petri-test-id\n",
  stderr: "",
  code: 0,
})

const sandbox = await Sandbox.create("base", {
  workspace: "/ws",
  policy: "policy.toml",
  runner: mockRunner,
})
```

## Reserved modules (v1)

`sandbox.files`, `sandbox.git`, and `sandbox.pty` are reserved for future
releases. Accessing them throws `NotImplementedError` in v1.
