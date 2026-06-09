# PROJECT STATE
_Last updated: 2026-06-09 by /close (#15 spore-core integration contract complete)_

## Current State
Petri is an early-stage microVM sandbox for running untrusted agent
workloads. It comprises a host `petri` CLI/lib, a `petri-guest` agent
(policy enforcement + vsock NDJSON dispatch), and a `petri-vz` Swift
helper driving Apple Virtualization. Working today: VM lifecycle state
machine, workspace mounting, shared protocol schema, LSP-backed semantic
tools, and the two-axis capability policy model:

- **command axis** (none<read_only<edit<yolo) with runtime `set_mode`
  escalation bounded by a boot-declared ceiling.
- **network axis** (none<allowlist<full), enforced **in-guest** (#36, done):
  an nftables ruleset applied at boot and live on `set_mode`, plus an in-guest
  DNS proxy that resolves allowlisted domains and populates TTL-scoped nft sets
  (IP/CIDR enforced directly; domains via the proxy). Tool processes run
  unprivileged (`agent` uid 1000, per-child privilege drop) so they cannot
  touch the ruleset. systemd-networkd brings the guest NIC up via DHCP so egress
  actually functions; the host policy file is `0o600`. VM-verified end-to-end.

Dispatch is now resilient to transient transport errors (#36 follow-up
#32, done): a flaky connect/read/write or a guest-reported error returns
the instance to `Ready` instead of bricking it to `Failed`, and the same
fix removed a path that stranded instances in `RunningDispatch` (errors
used to `?`-escape before the recovery step). `Failed` is now reserved for
genuinely unrecoverable states.

The lifecycle state file is now concurrency-safe (#33, done): a
per-instance advisory lock (`flock(LOCK_EX)` on `instance.lock`; no-op on
non-Unix) is held across the **entire** `dispatch`/`stop`/`teardown`
operation, so concurrent drivers serialize on an instance instead of
interleaving the `load_state → transition → write_state` read-modify-write.
A second `dispatch` blocks until the first returns the instance to `Ready`,
eliminating the spurious `invalid lifecycle transition` errors. (`create`
is deliberately left unlocked; concurrent same-ID create is a user-error
case outside this fix's scope.)

Vsock/control-socket framing now reads in 64 KB chunks instead of one
`read()` syscall per byte (#34, done): a `BufferedReader` scans each chunk
for the `\n` delimiter and retains over-read bytes, cutting a 4 MB dispatch
result from ~4M syscalls to ~64. Applied to both the guest dispatch
(`sendFrameToGuest`) and control-socket (`ControlServer.handle`) paths;
EOF/EINTR/error semantics preserved. Builds clean; framing-only change, not
yet re-exercised against a booted VM.

The wire protocol now has a published, enforced contract (#24, done): a
single Rust source of truth (`petri-protocol` crate, shared host+guest), a
checked-in JSON Schema (`schema/petri-protocol-v1.schema.json`), NDJSON
framing/correlation docs, versioning rules, fixtures, and an SDK-module map
(Sandbox/Commands/Filesystem/Git/Pty/Template). As of `8eb4452` the schema is
enforced rather than illustrative: a `petri-protocol` test compiles the schema
and validates every fixture against it, validates frames built from the Rust
constructors (catching schema↔type drift), and asserts invalid frames are
rejected. Generating first-party client types from the schema is deferred to
#26.

The E2B-style SDK shape and CLI now exist (#27/#29, done, `8a47c4c`). The CLI
exposes `petri sandbox create|list|connect|exec|kill` with the old commands
(`create`/`dispatch`/`stop`/`teardown`/`image build`) kept as compatibility
aliases; `sandbox exec` supports `--cwd`/`--env`/`--timeout-ms`/
`--max-output-bytes` and stdin passthrough, and `sandbox connect` is a
non-interactive readiness check (interactive PTY attach deferred). The SDK is a
documented language-agnostic contract (`docs/sdk-api.md`) plus a Rust reference
implementation (`crates/petri/src/sdk.rs`): a `Sandbox` over `HostBackend` with
`create`/`connect`/`list`/`kill` and a `commands().run()` returning a typed
`CommandResult`, exercised by unit tests against an in-memory fake backend.
`files`/`git`/`pty` are named and reserved but unimplemented in v1. SDK/CLI
`metadata` is now fully wired through (follow-up to #27/#29, done): it is
persisted into `instance.json` (`RuntimeState`/`InstanceConfig`/`InstanceHandle`
all carry it), surfaced on listed handles, rehydrated on `Sandbox::connect`, and
filterable via `sandbox list --metadata` (and a new `sandbox create --metadata`
flag) instead of the old behavior where any `--metadata` filter cleared the
result set.

The hardened dispatch path now has an automated guard (#14, done, `f5dcd24`):
`crates/petri/tests/e2e_dispatch.rs` boots a real microVM through the public SDK
(`Sandbox`/`MacosBackend`) and asserts the full round trip — a command's stdout
returns over vsock, a host-seeded workspace file is read from the guest, a
guest-written file is observed on the host, and the default `network_enabled =
false` policy leaves only loopback attached (`ls /sys/class/net` == `lo`). A
`VmGuard` Drop guard tears the VM down even on a failed assertion. Following the
`lsp_real_server.rs` convention it is `#[ignore]`d and skips gracefully when
prerequisites are absent (macOS, a codesigned `petri-vz`, a built base image
bundle), so plain `cargo test` stays green everywhere. VM-verified: passes
against a real VM in ~2.2s. Still missing for the breadth phase: generated
first-party client packages (#26).

A low-severity code-review polish pass is done (#35, `c4ac941`): workload
processes now launch with a clean environment (`env_clear` + minimal PATH
baseline) instead of inheriting the guest agent's full env; `terminate_process`
signals via `libc::kill` instead of spawning `/bin/kill`; `jsonrpc::read_message`
caps the buffer it allocates from an untrusted LSP `Content-Length`; the dead
`Policy::cwd_is_in_workspace` was removed; and the workspace is clippy-clean.
Builds clean, all tests pass; the env change is covered by a new in-process unit
test (the VM-backed e2e test was not re-run).

The spore-core consumption boundary now has a published contract (#15, done,
`cf0e257`): `docs/spore-core-integration.md` specifies the `PetriSandboxProvider`
boundary end-to-end — Petri discovery/startup (binary resolution, host
preconditions, the create boot-hang backstop), session lifecycle + state mapping
(ready/busy/dead, one-in-flight-dispatch serialization), tool dispatch mapping
(`commands.run`/`bash_command`, clean-env per #35, workspace/`cwd`), result
mapping (the load-bearing rule that non-zero exit is *not* an integration error),
the three error classes + typed-error family, and failure-recovery expectations
(Petri's #32/#33 guarantees vs. provider retry/teardown/orphan-reaping
obligations). It mandates building the provider on a first-party client (#26) so
the future HTTP control plane swaps underneath unchanged. Documentation-only
deliverable; the actual provider implementation lives in the spore-core repo
(out of scope here). Linked from ADR 0001 and README.

First-party client packages now exist for all four languages (#26, done,
`5f16cf8`): the Rust reference (`crates/petri/src/sdk.rs`) plus thin
TypeScript/Python/Go clients under `clients/`, each exposing the same
E2B-style `Sandbox` surface (`create`/`connect`/`list`/`kill`,
`commands.run`, reserved `files`/`git`/`pty`) and a consistent typed-error
family (not-found, not-ready, policy-denied, timeout, output-truncated,
command-failed, protocol-version-mismatch). Because there is no HTTP control
plane yet, the non-Rust clients are thin wrappers over the `petri` CLI: each
call shells out to `petri sandbox ...`, captures stdout/stderr/exit, and
parses the CLI's JSON. An injectable runner makes the whole surface testable
without a binary or VM — TS 52 / Python 61 / Go 43 tests pass (all against
mock runners; none exercised against a booted VM). The shared transport +
CLI-mapping contract is in `clients/README.md`. Deferred per the issue body:
the `files`/`git`/`pty` operations, and registry publishing (clients install
from path today).

Only the macOS/Apple Virtualization backend exists. Known incomplete /
broken: guest cancellation is unimplemented. Operational flake: `petri sandbox create` intermittently
hangs (~30 min, 0 CPU, no instance dir) — workaround is `pkill -9 -f
"sandbox create"; pkill -9 -f petri-vz` and retry. This hang predates
dispatch and was historically linked to #32/#33; both are now fixed, so the
next occurrence needs fresh diagnosis rather than being attributed to the
old reliability bugs. Network domain filtering is good-faith, not a hard
per-domain guarantee (shared-CDN-IP and DoH bypasses remain; ADR 0002).

## Active Direction
An E2B-style sandbox you run on your own hardware (local or self-hosted
server), with a hardened policy model — boot-declared capability ceilings
and runtime mode switching — and eventually a remote HTTP control plane.
Near-term focus: all three named dispatch-reliability bugs are now done
(#32 dispatch error recovery, #33 state-file locking, #34 vsock read
scaling), so the host↔guest dispatch path is hardened and the path is clear
to broaden surface. The shared protocol schema (#24) is now published and
enforced, so the breadth phase has its contract, and the E2B-style CLI/SDK
shape (#27/#29) now sits on top of it, the hardened dispatch path is locked by
an automated real-VM end-to-end test (#14), first-party clients for all four
languages ship (#26), and the spore-core consumption boundary is now a published
contract (#15). The breadth + contract phase is complete: the surface is
contracted, implemented, client-accessible, and the integration boundary is
specified. Consumption proper — implementing the `PetriSandboxProvider` against
this contract — now proceeds in the **spore-core repo** (out of scope here). The
levers remaining in *this* repo are reliability (re-diagnose the `sandbox create`
boot hang if it recurs; optionally guard #14 in macOS CI) and breadth of backends
toward the multi-platform north star (Linux Firecracker #16, then Windows
Hyper-V #18).

## Known Deviations
1. #31 was resolved more strongly than its `documentation` label implied:
   instead of documenting the shell-allowlist footgun, we built a command
   capability axis with runtime escalation (ADR 0002).
2. #36 enforcement pivoted host-side → in-guest, and is now **delivered**.
   The host-side spike (smoltcp over a `VZFileHandle` attachment) worked but
   ran ~130× slower than Apple NAT (~350 Mbit/s vs ~46 Gbit/s; spike doc 0036),
   so the design moved to in-guest nftables + DNS proxy + unprivileged tool
   execution. ADR 0002 revised; the spike code is parked in `git stash`. Issue
   #36's title still says "host-enforced" (superseded by the pivot).
3. #32's fix landed broader than the issue specified: beyond transient
   transport errors, guest-reported (`HelperResponse::Error`) failures are
   also treated as recoverable (the VM answered, so it's alive), and a
   second latent facet was fixed — transport errors used to `?`-escape
   `dispatch` before recovery, stranding instances in `RunningDispatch`.
4. Discovered + fixed during #36 verification: sandbox images never brought up
   the network interface, so in-guest egress was non-functional out of the box
   on any prior image. Fixed by baking a systemd-networkd DHCP config into the
   base image (`14c9c78`); shipped base image rebuilt (`5a7055e`). Rebuild also
   fixed a chain of latent base-image build bugs (chroot mounts/env, TOML-array
   comment parsing, 2G→8G disk).
5. #33's lock guards `dispatch`/`stop`/`teardown` but intentionally not
   `create`. The issue named the three lifecycle ops; concurrent same-ID
   `create` is a distinct user-error case and was left out of scope to keep
   the fix tight. Revisit if a control plane ever issues creates concurrently.
6. #34 was implemented as a reusable `BufferedReader` class (retains bytes
   read past a frame's delimiter) rather than an inline per-call buffer. Both
   current call sites read exactly one frame per connection then close, so the
   retention isn't exercised today — it's there to keep the reader correct if a
   future caller reads multiple frames over one fd.
7. #24 closed for protocol-definition scope only. The schema is published and
   enforced, but generating actual first-party client *types* from it (the
   "language clients can use the schema" criterion) was split out to #26, and
   the SDK API shape that consumes it to #27. The schema also reserves planned
   operation names (Filesystem/Git/Pty/Template) that have no runtime handler
   yet — intentional, to let clients align naming ahead of implementation.
8. #27/#29 shipped the SDK/CLI shape but several named-and-reserved pieces are
   intentionally deferred per the issue bodies: `sandbox connect` is a
   non-interactive readiness check (no PTY attach), `exec --background`/`--user`
   error as not-yet-implemented, and the SDK `files`/`git`/`pty` modules plus
   `setTimeout`/`setPolicy`/snapshots are named but unimplemented. (The SDK
   `metadata` gap noted here previously is now closed — see Current State; it is
   persisted and filterable.)
9. #35 (low-severity polish) landed one behavior change worth flagging:
   workload processes now run with a clean environment (`env_clear` + a minimal
   PATH baseline) instead of inheriting the guest agent's full env. A command
   that previously relied on an inherited variable other than PATH must now
   receive it explicitly in the request env. The rest of #35 was hygiene
   (libc::kill teardown, bounded LSP Content-Length, dead-code removal, clippy).
10. #14's e2e test requires two host-setup steps that are easy to miss and are
   documented in the test header: (a) the `petri-vz` helper must be codesigned
   with `crates/petri-vz/petri-vz.entitlements` — an unsigned helper is rejected
   at VM-config time with a "com.apple.security.virtualization entitlement"
   error; (b) the macOS ~104-byte Unix-socket path cap means the backend's
   control socket (`<state_dir>/<id>/petri-vz.sock`) overflows under long temp
   paths, so the test uses a short `/tmp` base dir and short instance id. Both
   are test/runtime concerns only; neither changes product code.
11. #26 closed for the issue's stated scope (thin client + lifecycle + command
   execution + typed errors + reserved module names) — not its aspirational
   ceiling. The clients are **CLI-subprocess wrappers**, not generated off the
   JSON Schema as #24 originally imagined: with no HTTP control plane and a
   macOS-only in-process backend, shelling out to `petri` was the only viable
   transport today. The `files`/`git`/`pty` operations and registry publishing
   are deferred per the issue body, and client tests run only against mock
   runners (never a booted VM). The "spore-core providers can use these"
   criterion is satisfied in principle but the actual wiring is #15.

## Next Actions
Breadth + contract phase is complete: protocol contract (#24) enforced, SDK/CLI
shape (#27/#29) on top, dispatch locked by a real-VM e2e test (#14), first-party
clients shipped (#26), and the spore-core integration contract published (#15).
Consumption proper (the `PetriSandboxProvider`) now happens in the spore-core
repo. The remaining levers here are reliability and backend breadth — pick the
next based on whether spore-core integration surfaces a reliability blocker first.
1. #16 — implement the Linux Firecracker/KVM backend. Highest-leverage next step
   toward the "run on your own hardware" north star: today only the macOS/Apple
   Virtualization backend exists, which blocks self-hosted-server use. The
   protocol/SDK/policy contracts are all backend-agnostic now, so this is a
   well-scoped second backend behind a settled boundary.
2. Re-diagnose the `sandbox create` hang if it recurs. Historical links
   (#32/#33) are all fixed, so it can no longer be attributed to them; needs a
   fresh root-cause pass (likely in the Swift helper boot/ready path). Promote to
   #1 if spore-core integration hits it in practice.
3. Consider wiring #14's e2e test into a macOS CI lane (build + codesign
   `petri-vz`, build the base image, run `--ignored`) so the dispatch path is
   guarded automatically rather than only on demand.
4. Follow-ups spun off #26 when there's demand: implement the reserved
   `files`/`git`/`pty` client operations, and publish the clients to
   npm/PyPI/the Go module proxy (they install from path today).
5. #28 — per-instance capability tokens for command authorization. The contract
   (#15) and protocol already reserve `capability_denied`/`AuthorizationError`;
   this fills in the enforcement behind that reserved surface.
