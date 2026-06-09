# Petri Sandbox SDK API (v1)

This is the language-agnostic contract for Petri's high-level sandbox SDK
(issue #27). It models an E2B-style `Sandbox` object so users do not hand-roll
the wire protocol. The contract is designed to be implemented consistently in
Rust, TypeScript, Python, and Go; the Rust implementation in
[`crates/petri/src/sdk.rs`](../crates/petri/src/sdk.rs) is the reference.

All SDK calls ultimately produce protocol frames defined by the shared schema
in [`schema/petri-protocol-v1.schema.json`](../schema/petri-protocol-v1.schema.json)
and described in [protocol-schema.md](protocol-schema.md). The SDK is the
recommended path; raw protocol access stays possible underneath (the Rust
`Sandbox::backend()` escape hatch, or constructing `DispatchRequest` directly).

## Target shape

```ts
import { Sandbox } from "@squirrelsoft/petri"

const sandbox = await Sandbox.create("base", {
  workspace: ".",
  policy: "./policy.toml",
})

const result = await sandbox.commands.run("cargo test", { cwd: "/workspace" })

await sandbox.kill()
```

The same shape in Rust:

```rust
use petri::{PetriBackend, Sandbox, SandboxOptions, CommandOptions};

let backend = PetriBackend::default();
let sandbox = Sandbox::create(
    backend,
    SandboxOptions::new("/abs/workspace", "policy.toml"),
)?;

let result = sandbox.commands().run("cargo test", CommandOptions {
    cwd: Some("/workspace".into()),
    ..Default::default()
})?;

sandbox.kill()?;
```

In a future async language binding, every method is `async`/awaitable. The Rust
reference is synchronous against the local backend; that is an implementation
detail of the binding, not of the contract.

## `Sandbox`

### Properties

| Property | Type | v1 |
|---|---|---|
| `sandbox.sandboxId` | string | ✅ |
| `sandbox.commands` | `Commands` | ✅ |
| `sandbox.files` | `Filesystem` | reserved |
| `sandbox.git` | `Git` | reserved |
| `sandbox.pty` | `Pty` | reserved |

In Rust, modules are accessor methods (`sandbox.commands()`) and `sandboxId` is
`sandbox.id()`, because Rust cannot hold a self-borrowing field. The semantics
are identical.

### Static methods

| Method | v1 | Notes |
|---|---|---|
| `Sandbox.create(opts)` / `Sandbox.create(template, opts)` | ✅ | `template` defaults to `base` |
| `Sandbox.connect(sandboxId, opts?)` | ✅ | attaches to a running sandbox; never tears it down |
| `Sandbox.list(opts?)` | ✅ | returns sandbox info records |
| `Sandbox.kill(sandboxId, opts?)` | ✅ | tears the sandbox down |
| `Sandbox.getInfo(sandboxId, opts?)` | ✅ | returns the lifecycle handle, or none |
| `Sandbox.getMetrics(...)` | later | — |
| `Sandbox.pause(...)` / `Sandbox.createSnapshot(...)` | later | — |

### Instance methods

| Method | v1 | Notes |
|---|---|---|
| `sandbox.commands.run(command, opts?)` | ✅ | see `Commands` |
| `sandbox.getInfo(opts?)` | ✅ | current lifecycle handle |
| `sandbox.isRunning(opts?)` | ✅ | `true` when state is `ready` or `running_dispatch` |
| `sandbox.kill(opts?)` | ✅ | tears the sandbox down |
| `sandbox.connect(opts?)` | ✅ | re-attach / refresh |
| `sandbox.setTimeout(...)` / `setPolicy(...)` / `createSnapshot(...)` | later | when runtime updates land |

## Options

```ts
type SandboxOpts = {
  id?: string                          // generated when omitted
  workspace?: string                   // required by the local backend in v1
  policy?: string | Policy             // required by the local backend in v1
  backend?: "macos"                    // defaults to "macos"
  image?: string                       // defaults to the backend's base image
  metadata?: Record<string, string>    // persisted with the instance; filterable on list
  timeoutMs?: number                   // later
  requestTimeoutMs?: number            // later
}
```

`workspace` and `policy` are optional in the cross-language type but **required**
by the current local Petri backend; a binding may default them (e.g. `workspace`
to the current directory) but must ultimately supply both. `metadata` is
persisted with the instance state by the local backend, surfaced on listed
handles, and filterable via `list({ metadata })` / `sandbox list --metadata`.

## `Commands`

```ts
type CommandOpts = {
  cwd?: string                 // defaults to /workspace
  args?: string[]              // appended after the command
  env?: Record<string, string>
  stdin?: string               // piped to the process
  user?: string                // later
  timeoutMs?: number
  maxOutputBytes?: number
  requestId?: string           // generated when omitted
  background?: boolean         // later
}
```

| Method | v1 |
|---|---|
| `commands.run(command, opts?)` | ✅ |
| `commands.stream(command, opts?)` | later |

`run` maps to a `bash_command` dispatch request (`BashCommandRequest` in the
schema) and returns a typed `CommandResult`.

### `CommandResult`

```ts
type CommandResult = {
  status: "success" | "failure" | "rejected" | "timeout" | "cancelled" | "malformed"
  exitCode: number | null      // unwrapped one level vs. the raw frame
  stdout: string
  stderr: string
  outputTruncated: boolean
  error: ErrorFrame | null     // structured error for non-success statuses
}
```

`success` is a convenience predicate: `status === "success" && exitCode === 0`.
The result is the SDK view of the protocol `ResultFrame`/`DispatchResult`:
streams are flattened to strings (never null) and the doubly-wrapped
`exit_code` (`Option<Option<i32>>`) is unwrapped to a single nullable integer.

## Errors

SDK methods surface two error families:

- **Usage / config errors** — invalid id, missing workspace/policy, unknown
  backend. Raised before any dispatch.
- **Backend / transport / guest errors** — the sandbox is gone, not running, or
  the guest reported a failure. In Rust these are `PetriError` variants
  (`Backend`, `Transport`, `Guest`); other bindings map them to their idiomatic
  error type. Note that a *command* that runs and exits non-zero is **not** an
  error — it returns a `CommandResult` with a non-zero `exitCode`.

## Reserved modules (not implemented in v1)

The following modules are named and reserved so the surface is stable and type
generation can align, per the SDK module map in
[protocol-schema.md](protocol-schema.md). Their operations are deferred:

- `Filesystem`: `exists`, `getInfo`, `list`, `read`, `write`, `writeFiles`,
  `makeDir`, `remove`, `rename`, `watchDir` (later)
- `Git`: thin wrapper over command execution first; later `clone`, `status`,
  `diff`, `commit`, `checkout`
- `Pty`: `create`, `input`, `resize`, `close` (later, once interactive sessions
  land)
- Snapshots, pause/resume, signed upload/download URLs, `getHost(port)`,
  metrics, MCP URL/token, and the template builder DSL are all out of scope for
  v1.

## CLI alignment

The CLI mirrors this contract under `petri sandbox ...` (issue #29):

| SDK | CLI |
|---|---|
| `Sandbox.create` | `petri sandbox create [template]` |
| `Sandbox.connect` | `petri sandbox connect <id>` |
| `Sandbox.list` | `petri sandbox list` |
| `commands.run` | `petri sandbox exec <id> <command> [args...]` |
| `Sandbox.kill` | `petri sandbox kill [--all \| <id>...]` |
