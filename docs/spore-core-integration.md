# spore-core ↔ Petri Integration Contract (v1)

This document specifies how **spore-core** delegates sandboxed tool execution to
**Petri**, so that each side can be implemented independently against a fixed
boundary. It is the contract referenced by issue #15 and is the consumption-side
companion to the breadth-phase work: the protocol schema (#24), the SDK shape
(#27/#29), and the first-party clients (#26).

The integration is expressed as a **`PetriSandboxProvider`** inside spore-core —
the component named in [ADR 0001](adr/0001-petri-architecture.md). The provider
owns a Petri sandbox handle and routes tool execution through it instead of
running tools directly on the host. spore-core depends only on this contract,
never on a VM backend, guest OS, image format, or process-launch detail — Petri
owns those behind the boundary.

Authoritative surfaces this contract sits on top of:

- High-level SDK shape — [`docs/sdk-api.md`](sdk-api.md)
- First-party clients (TS / Python / Go / Rust) — [`clients/README.md`](../clients/README.md)
- Wire protocol — [`docs/protocol-schema.md`](protocol-schema.md) and
  [`schema/petri-protocol-v1.schema.json`](../schema/petri-protocol-v1.schema.json)

The provider **must** be built on a first-party client, not on hand-rolled
protocol or CLI code. That is the whole point of #26: one blessed transport, one
typed-error family, one place to swap in the HTTP control plane later.

---

## 1. Roles and boundary

| Side | Owns |
|---|---|
| **spore-core** | Tool-call intent (command, args, cwd, limits), the policy file it boots the sandbox with, session ownership, treating all returned output/workspace state as untrusted. |
| **Petri** | VM lifecycle, the guest agent, policy *enforcement*, vsock dispatch, result framing, the client packages. |

The boundary is a **client object**, not a network protocol. In v1 the client
shells out to the `petri` CLI on the same host (no remote control plane yet); the
provider neither knows nor depends on that. When the HTTP control plane lands,
only the client's transport changes and this contract is unaffected.

One `PetriSandboxProvider` instance corresponds to exactly **one** sandbox
(`Sandbox`) for its lifetime. A spore-core process that needs N concurrent
sandboxes holds N providers.

---

## 2. Petri discovery and startup

### 2.1 Locating Petri

The provider locates the Petri binary through the client's standard resolution
order (do not reimplement this):

1. explicit option (`petriPath` / `petri_path` / `PetriPath`),
2. the `PETRI_BIN` environment variable,
3. `petri` on `PATH`.

If none resolve, sandbox creation fails fast with a usage/config error (§6,
class A) **before** any VM work. The provider should surface this as
"Petri is not installed / not on PATH", not as a transient sandbox failure.

### 2.2 Host prerequisites

v1 Petri is **macOS + Apple Virtualization only**. The provider must treat a
non-macOS host, a missing/unsigned `petri-vz` helper, or a missing base-image
bundle as a **startup precondition failure** (class A), not a retryable error.
These are environment problems, not sandbox flakiness.

### 2.3 Creating the sandbox

The provider creates a sandbox via the client's `Sandbox.create(template, opts)`:

```
Sandbox.create("base", {
  workspace: <abs path to the session workspace>,   // required by local backend
  policy:    <path to a policy.toml | inline Policy>, // required by local backend
  metadata:  { sporeSession: <session id>, ... },     // optional, persisted+filterable
  id:        <stable id>,                              // optional; generated if omitted
})
```

Rules:

- `workspace` and `policy` are **required** by the current local backend even
  though they are optional in the cross-language type. The provider must always
  supply both (it may default `workspace` to the session's working directory).
- `template` defaults to `"base"`.
- `policy` declares the **capability ceiling** for the whole session (command
  axis `none<read_only<edit<yolo`, network axis `none<allowlist<full`). Runtime
  `set_mode` can escalate only *within* that boot-declared ceiling. The provider
  should pick the tightest ceiling the session needs, not the broadest.
- `metadata` is persisted into instance state, surfaced on listed handles, and
  filterable on `list({ metadata })`. Use it to tag sandboxes with the owning
  spore session so orphans can be found and reaped (§7.4).
- `create` returns once the sandbox reaches **`ready`**. The provider should not
  dispatch until it observes `ready` (a fresh handle reports `ready` on return).

### 2.4 The create boot-hang caveat

There is a known operational flake: `petri sandbox create` intermittently hangs
(no instance dir, 0 CPU). The provider **must** apply a creation timeout and, on
expiry, treat it as a class-B startup failure: surface a clear error, do not
leave a half-started provider in service. The recovery is to abandon the attempt
(kill any stray process out of band) and create again. The provider must not
silently block a spore session forever on a hung create.

---

## 3. Session lifecycle

A provider session is the span from a successful `create`/`connect` to `kill`.

### 3.1 Lifecycle states

Petri instances move through these states (snake_case on the wire):

```
provisioning → booting → ready ⇄ running_dispatch
                           │
                           ├── stopping → torn_down
                           └── failed
```

The provider maps them to a small ready/busy/dead model:

| Petri state | Provider view | Can dispatch? |
|---|---|---|
| `provisioning`, `booting` | starting | no (wait) |
| `ready` | available | **yes** |
| `running_dispatch` | busy (a dispatch is in flight) | no — serialize |
| `stopping`, `torn_down` | gone | no |
| `failed` | dead (unrecoverable) | no |

`isRunning()` is true for `ready` **and** `running_dispatch`. "Can accept a new
command" is true only for `ready`.

### 3.2 One in-flight dispatch per sandbox

A sandbox processes **one** dispatch at a time. Petri serializes concurrent
drivers with a per-instance advisory lock (#33), so a second dispatch *blocks*
until the first returns the instance to `ready` rather than erroring. The
provider should still avoid issuing concurrent `commands.run` against the same
sandbox; if its own callers can be concurrent, it serializes them itself and
relies on the Petri lock only as a backstop.

### 3.3 Attaching to an existing sandbox

`Sandbox.connect(id)` re-attaches to a running sandbox and **never** tears it
down. It is a readiness check: it errors if the sandbox is missing or not
running. Use it to resume control of a sandbox across provider restarts (pair
with `metadata`/`list` to rediscover the id).

### 3.4 Teardown

`kill(id)` (or `sandbox.kill()`) tears the sandbox down. The provider **owns**
teardown and must guarantee it on session end, including on its own error paths
(equivalent of the `VmGuard` Drop guard in the e2e test). `connect` callers that
only borrowed a sandbox must **not** kill it unless they are the owner.

`kill` should be idempotent from the provider's perspective: killing an
already-gone sandbox is a no-op success, not an error to propagate.

---

## 4. Tool dispatch mapping

spore-core tool calls map to `commands.run`, which maps to a `bash_command`
dispatch (`BashCommandRequest`).

### 4.1 Request mapping

| spore-core intent | SDK `CommandOpts` | Notes |
|---|---|---|
| command line | `run(command, …)` | the program/shell line |
| arguments | `args: string[]` | appended after `command` |
| working directory | `cwd` | **defaults to `/workspace`**; must be inside the workspace |
| environment | `env: Record<string,string>` | see §4.2 |
| stdin payload | `stdin: string` | piped to the child |
| wall-clock limit | `timeoutMs` | per-command; enforced in-guest |
| output cap | `maxOutputBytes` | truncation, not failure (§5) |
| correlation id | `requestId` | generated if omitted; echoed back as `id` |

Reserved / **not** available in v1 (the provider must not emit them; the CLI
rejects them): `user`, `background`, `commands.stream`.

### 4.2 Environment is not inherited

As of #35, workload processes launch with a **clean environment** (`env_clear` +
a minimal `PATH` baseline) — they do **not** inherit the guest agent's env. Any
variable a command needs beyond `PATH` must be passed explicitly in `env`. The
provider must not assume host env leaks into the guest.

### 4.3 Workspace contract

The workspace mounted at `/workspace` is the **only** durable, shared surface
between host and guest. spore-core seeds inputs by writing into the host-side
workspace before dispatch and reads results by observing the workspace after.
`cwd` must resolve inside `/workspace`. See
[`docs/workspace-contract.md`](workspace-contract.md).

### 4.4 Policy and network

Whether a command is allowed is decided **in-guest** by policy, not by the
provider. The provider does not pre-screen commands; it submits them and maps a
`rejected` result (§5/§6). Network egress is off by default (`network_enabled =
false` ⇒ loopback only) and, when enabled, enforced in-guest via nftables + a DNS
proxy. Domain filtering is good-faith, not a hard per-domain guarantee (ADR
0002) — the provider must not present allowlist mode as airtight isolation.

---

## 5. Result mapping

`commands.run` returns a `CommandResult` — the flattened SDK view of the
protocol `ResultFrame`:

```
status:          "success" | "failure" | "rejected" | "timeout" | "cancelled" | "malformed"
exitCode:        number | null
stdout:          string            // never null; "" when absent
stderr:          string            // never null
outputTruncated: boolean
error:           ErrorFrame | null  // structured detail for non-success statuses
success:         derived = status == "success" && exitCode == 0
```

Provider mapping rules:

- **A command that runs and exits non-zero is NOT an integration error.** It is a
  normal `CommandResult` with `status == "failure"` and a non-zero `exitCode`.
  The provider returns it to spore-core as a tool result, not an exception. This
  is the single most important rule: do not conflate "the tool failed" with
  "Petri failed".
- `success` is the convenience predicate; use it, don't re-derive it.
- `outputTruncated == true` can co-occur with `status == "success"`. The provider
  must propagate the truncation flag to spore-core (so the model/user knows
  output was cut) rather than dropping it.
- `stdout`/`stderr` are already flattened to strings; `exit_code`'s
  double-`Option` wrapping is already unwrapped to a single nullable integer. The
  provider should not re-parse the raw frame.
- `status` values and their meaning:
  | status | meaning | provider handling |
  |---|---|---|
  | `success` | ran, exit 0 | normal result |
  | `failure` | ran, non-zero exit | normal result (carry exitCode) |
  | `rejected` | policy denied before/at execution | result + map to `PolicyDeniedError` on `check()` |
  | `timeout` | exceeded `timeoutMs` | result; `error.code = timeout_exceeded` |
  | `cancelled` | dispatch cancelled | result (note: guest cancellation is currently unimplemented) |
  | `malformed` | request rejected as malformed | usually a provider bug — surface loudly |

### 5.1 `check()` / `raise_for_status()`

The result carries an opt-in `check()` helper that raises the first applicable
typed error in this order: protocol mismatch → rejected/policy → timeout →
command-failed → truncated (truncation last, since it can ride on success). The
provider chooses per call whether spore-core wants exceptions-on-failure or
inspect-the-result semantics; **default to inspecting the result** and only
`check()` where spore-core's tool model wants a thrown error.

---

## 6. Error handling

Errors fall into three classes. The provider must keep them distinct because
they have different recoveries.

**Class A — usage / config (raised before any dispatch).**
Invalid id, missing workspace/policy, unknown backend, Petri binary not found,
host prerequisite missing. These are deterministic; retrying unchanged will not
help. Surface to spore-core as a setup error.

**Class B — backend / transport / guest (the sandbox is gone, not running, or
the guest reported a failure).** Mapped to the typed-error family:

| Typed error | Trigger |
|---|---|
| `SandboxNotFoundError` | no sandbox with that id |
| `SandboxNotReadyError` | sandbox exists but not running |
| `PolicyDeniedError` | `status == rejected` / `error.code == policy_denied` |
| `CommandTimeoutError` | `status == timeout` / `timeout_exceeded` |
| `OutputTruncatedError` | `outputTruncated == true` |
| `CommandFailedError` | `status == failure` (non-zero exit), via `check()` |
| `AuthorizationError` | reserved auth/capability codes (e.g. `capability_denied`) |
| `ProtocolVersionMismatchError` | `frame.protocol_version != 1` |
| `PetriError` (base) | any other Petri failure |

**Class C — command outcome (NOT an integration error).** Non-zero exit,
stderr output, partial/truncated output. These are *data*, returned as a
`CommandResult`. The provider only converts them to exceptions if spore-core
explicitly opts in via `check()`.

Canonical wire `error.code` values the provider may see: `policy_denied`,
`timeout_exceeded`, `invalid_request`, `unsupported_protocol_version`, and the
reserved `capability_denied`. The provider should map by code where it
recognizes one and fall back to `PetriError` otherwise — never crash on an
unknown code.

`ProtocolVersionMismatchError` is special: it means Petri and the client
disagree on the wire version. It is **not** retryable and indicates a
version-skew deployment bug; surface it as fatal for the session.

---

## 7. Failure recovery expectations

This section defines what each side guarantees so neither over- nor under-reacts
to a fault.

### 7.1 Petri's guarantees

- **Transient transport faults are recoverable (#32).** A flaky
  connect/read/write, or a guest-reported (`HelperResponse::Error`) failure,
  returns the instance to `ready` — it does **not** brick it to `failed`. The
  guest answered, so it is alive. `failed` is reserved for genuinely
  unrecoverable states.
- **No stranding in `running_dispatch` (#32).** The path that used to `?`-escape
  before recovery is fixed; a faulted dispatch lands back on `ready`.
- **Concurrency is serialized (#33).** A per-instance advisory lock spans the
  whole dispatch/stop/teardown op, so concurrent drivers serialize instead of
  corrupting the state file. (`create` is intentionally *not* locked; concurrent
  same-id create is out of scope — the provider must not issue it.)

### 7.2 Provider obligations on a faulted dispatch

1. **Re-check state, don't assume death.** On a class-B transport error, call
   `getInfo()`/`isRunning()`. If the sandbox is back to `ready`, the dispatch is
   safely **retryable** (Petri already recovered the instance).
2. **Retry idempotently and bounded.** Only retry commands that are safe to
   re-run, with a small bounded count and backoff. A command with side effects
   in the workspace is not automatically idempotent — let spore-core decide.
3. **Treat `failed` as terminal.** A `failed` sandbox is dead; the provider must
   not keep dispatching to it. Tear it down and, if the session must continue,
   create a fresh one.
4. **Reuse the correlation id on retry** so duplicate results can be detected.

### 7.3 Timeouts and cancellation

- Per-command `timeoutMs` is enforced **in-guest**; a timeout returns a
  `CommandResult` (`status == timeout`), not a hung call. The provider should
  still apply its own outer deadline as a backstop against a wedged transport.
- **Guest-side cancellation is currently unimplemented.** The provider must not
  rely on cancelling an in-flight command; design around per-command timeouts.
  When cancellation lands, it slots in behind the existing `cancelled` status.

### 7.4 Orphans and cleanup

- The provider guarantees teardown on session end and on its own crash paths.
- Because a crashed provider can leave a sandbox running, tag every sandbox with
  session `metadata` at create time and provide a reaper that `list({ metadata })`s
  by session and `kill`s orphans. This is the supported way to find and reclaim
  leaked sandboxes.
- Teardown is idempotent: killing an already-gone sandbox is success.

### 7.5 The create-hang backstop

Per §2.4, `create` can hang. The provider's recovery is a creation timeout +
abandon-and-recreate, never an unbounded wait. This is the one startup fault that
is *not* covered by Petri's internal recovery guarantees and must be handled in
the provider.

---

## 8. Versioning and stability

- The wire protocol is **v1**; every frame carries `protocol_version: 1`. A
  mismatch is fatal (§6).
- The `Sandbox` surface is stable. `files`/`git`/`pty` are reserved and raise
  "not implemented in v1"; the provider must not depend on them yet.
- When the HTTP control plane arrives, the client's transport changes but this
  contract — discovery, lifecycle, dispatch/result mapping, error classes,
  recovery expectations — is intended to hold unchanged. New capabilities
  (streaming, PTY attach, snapshots, cancellation) extend the contract; they do
  not alter the v1 guarantees above.

## 9. Done-when checklist (issue #15)

This contract is "documented clearly enough for Petri and spore-core to implement
independently" when a reader can, from this file alone, answer:

- [x] How does the provider find and start Petri, and what counts as a startup
  precondition vs. a flake? (§2)
- [x] What states does a session move through, and when can it dispatch? (§3)
- [x] How does a spore-core tool call map onto a dispatch request? (§4)
- [x] How does a dispatch result map back, and what is *not* an error? (§5)
- [x] What are the error classes and the typed-error family? (§6)
- [x] What does each side guarantee on failure, and what must the provider do
  about it? (§7)

Implementation of the provider lives in spore-core and is tracked there; this
repo owns the contract and the first-party clients it stands on.
