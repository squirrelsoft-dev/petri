# Petri SDK Guide

How to drive Petri sandboxes from code in Rust, TypeScript, Python, and Go. This
is the *how-to*; the language-agnostic **contract** (every method, option, and
result field) lives in [Sandbox SDK API](sdk-api.md), and the CLI/transport
mapping in [`clients/README.md`](../clients/README.md).

All four clients expose the same E2B-style `Sandbox` surface. The non-Rust
clients are thin wrappers that shell out to the `petri` CLI and parse its JSON;
the Rust client drives an in-process backend directly. When the HTTP control
plane lands, only the transport changes — your code doesn't.

## Prerequisites & binary resolution

The non-Rust clients need the `petri` binary. Each resolves it in this order:

1. an explicit option — `petriPath` (TS) / `petri_path=` (Python) / `PetriPath`
   (Go),
2. the `PETRI_BIN` environment variable,
3. `petri` on `PATH`.

Running real sandboxes also requires the macOS backend (see
[CLI Guide](cli-guide.md)). Every client accepts an injectable **runner** so you
can unit-test the full surface without a binary or VM — see
[Testing](#testing-without-a-binary).

## Install

| Language | Install | Import |
|---|---|---|
| TypeScript | `npm install @squirrelsoft/petri` | `import { Sandbox } from "@squirrelsoft/petri"` |
| Python (3.10+) | `pip install petri-sandbox` (or `pip install -e clients/python`) | `from petri import Sandbox` |
| Go | `go get github.com/squirrelsoft/petri-go` | `import petri "github.com/squirrelsoft/petri-go"` |
| Rust | depend on the `petri` crate | `use petri::{PetriBackend, Sandbox, SandboxOptions, CommandOptions};` |

## Create a sandbox, run a command, tear down

The core loop in each language. `template` defaults to `base`; `workspace` and
`policy` are required by the local backend.

For `policy`, the **CLI-backed clients (TypeScript, Python, Go)** accept either a
**template name** (`developer`, `locked-down`, `yolo`, `fetch`, or your own) or a
path to a `.toml` file — they pass the value to `petri sandbox create`, which
resolves names through the [policy registry](cli-guide.md#choosing-a-policy). The
**in-process Rust client** drives the backend directly and currently expects a
policy **file path**; resolve a template to a path first with
`petri policy path <name>`.

**TypeScript** (every call is `async`):

```ts
import { Sandbox } from "@squirrelsoft/petri"

const sandbox = await Sandbox.create("base", {
  workspace: ".",
  policy: "developer",
})

const result = await sandbox.commands.run("cargo test", { cwd: "/workspace" })
console.log(result.stdout, result.exitCode, result.success)

await sandbox.kill()
```

**Python** (synchronous):

```python
from petri import Sandbox

sandbox = Sandbox.create("base", workspace=".", policy="developer")

result = sandbox.commands.run("cargo test", cwd="/workspace")
print(result.stdout, result.exit_code)

sandbox.kill()
```

**Go** (context-first):

```go
ctx := context.Background()

sb, err := petri.Create(ctx, "base", petri.CreateOptions{
    Workspace: ".",
    Policy:    "developer",
})
if err != nil { log.Fatal(err) }
defer sb.Kill(ctx)

result, err := sb.Commands().Run(ctx, "cargo test", petri.RunOptions{
    Cwd:       "/workspace",
    TimeoutMs: 60_000,
})
if err != nil { log.Fatal(err) } // transport/protocol error
fmt.Println(result.Stdout)
```

**Rust** (synchronous, in-process backend):

```rust
use petri::{PetriBackend, Sandbox, SandboxOptions, CommandOptions};

let backend = PetriBackend::default();
// In-process Rust takes a policy file path (not a template name).
let sandbox = Sandbox::create(
    backend,
    SandboxOptions::new("/abs/workspace", "./policy.toml"),
)?;

let result = sandbox.commands().run("cargo test", CommandOptions {
    cwd: Some("/workspace".into()),
    ..Default::default()
})?;

sandbox.kill()?;
```

In Rust, modules are accessor methods (`sandbox.commands()`) and the id is
`sandbox.id()`, because Rust can't hold a self-borrowing field; the semantics
match the other bindings.

## Command options

`run` maps to a `bash_command` dispatch. The options (camelCase in TS, snake or
kwargs in Python, fields on `RunOptions` in Go, `CommandOptions` in Rust):

| Option | Meaning |
|---|---|
| `cwd` | working directory inside the guest (defaults to `/workspace`) |
| `args` | extra arguments appended after the command |
| `env` | environment variable overrides |
| `stdin` | piped to the process's stdin |
| `timeoutMs` | per-request wall-clock timeout (≤ the policy cap) |
| `maxOutputBytes` | output cap before truncation (≤ the policy cap) |
| `requestId` | explicit correlation id (generated when omitted) |

`background` and `user` are reserved and rejected by v1.

## Results vs. exceptions

A command that **runs and exits non-zero is not an error** — it returns a normal
`CommandResult`. `run` throws/returns-error only for transport, usage, or
protocol failures (binary missing, malformed JSON, `protocol_version != 1`).

`CommandResult` fields: `status` (`success | failure | rejected | timeout |
cancelled | malformed`), `exitCode` (nullable), `stdout`, `stderr`,
`outputTruncated`, `error` (structured `ErrorFrame` for non-success), and the
derived `success` (`status == success && exitCode == 0`).

To opt **into** exceptions for policy-denied / timeout / failure / truncation,
call the result's check helper:

| Language | Helper |
|---|---|
| TypeScript | `result.check()` — throws, or returns the result |
| Python | `result.raise_for_status()` |
| Go | `if err := result.Check(); err != nil { … }` |
| Rust | `result.check()?` |

`check()` raises the first applicable error in order: protocol mismatch →
rejected/policy → timeout → command-failed → truncated; it's a no-op on a clean
success.

### Error types

A single base error (`PetriError`) with consistent subclasses/sentinels across
languages. The full table is in [`clients/README.md`](../clients/README.md#typed-errors);
the common ones:

| Concept | When |
|---|---|
| `SandboxNotFound*` | CLI stderr contains `no sandbox with id` |
| `SandboxNotReady*` | CLI stderr contains `not running` |
| `PolicyDenied*` | `check()` and status `rejected` / `error.code == policy_denied` |
| `CommandTimeout*` | `check()` and status `timeout` |
| `CommandFailed*` | `check()` and status `failure` (non-zero exit) |
| `OutputTruncated*` | `check()` and output truncated |
| `ProtocolVersionMismatch*` | frame `protocol_version != 1` |

(Go exposes these as `errors.Is` sentinels, e.g. `ErrPolicyDenied`.)

## Lifecycle: connect, list, info, kill

Beyond create/run/kill, each client exposes attach and introspection. Names
follow each language's convention (camelCase / snake_case / Go methods):

```ts
const sandbox = await Sandbox.connect("petri-abc123")   // attach; never tears down
const running = await Sandbox.list({ state: "running" }) // SandboxInfo[]
await Sandbox.kill("petri-abc123")                       // static teardown
const info = await sandbox.getInfo()                     // SandboxInfo | null
const isUp = await sandbox.isRunning()                   // ready | running_dispatch
```

```python
sandbox2 = Sandbox.connect(sandbox.sandbox_id)
handles  = Sandbox.list(state="running")     # list of SandboxInfo
Sandbox.kill(sandbox.sandbox_id)
info = sandbox.get_info()
up   = sandbox.is_running()
```

```go
sb2, _   := petri.Connect(ctx, id, petri.ConnectOptions{})
infos, _ := petri.List(ctx, petri.ListOptions{State: "running"})
_         = petri.Kill(ctx, id, petri.KillOptions{})
```

`metadata` set at create time (`{ metadata: { project: "acme" } }`) is persisted
with the instance, returned on `SandboxInfo`, and filterable via `list`.

## Testing without a binary

Inject a fake runner `(argv, stdin) -> (stdout, stderr, code)` to exercise the
full surface in tests without spawning `petri` or booting a VM:

```ts
import { Sandbox, Runner } from "@squirrelsoft/petri"
const mockRunner: Runner = async (argv, stdin) => ({ stdout: "petri-test-id\n", stderr: "", code: 0 })
const sandbox = await Sandbox.create("base", { workspace: "/ws", policy: "p.toml", runner: mockRunner })
```

```python
def fake_runner(argv, stdin=None):
    return "sb-test\n", "", 0
sandbox = Sandbox.create(runner=fake_runner)
```

```go
sb := &petri.Sandbox{SandboxID: "test-sb", Runner: myFakeRunner}
```

## Reserved modules (v1)

`sandbox.files`, `sandbox.git`, and `sandbox.pty` are named on the surface so the
API stays stable as they land, but raise a "not implemented in v1" error today.
Snapshots, pause/resume, metrics, port forwarding, and the template-builder DSL
are likewise out of scope for v1 — see [Sandbox SDK API](sdk-api.md#reserved-modules-not-implemented-in-v1).

## Raw protocol access

The SDK is the recommended path, but the wire protocol stays reachable: in Rust
via the `Sandbox::backend()` escape hatch or by constructing `DispatchRequest`
directly; from any language via `petri dispatch` (the only route to the `lsp_*`
tools). See [Vsock Dispatch Protocol](vsock-dispatch-protocol.md) and
[Protocol Schema](protocol-schema.md).
