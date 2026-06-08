# PROJECT STATE
_Last updated: 2026-06-07 by /close_

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

Only the macOS/Apple Virtualization backend exists. Known incomplete /
broken: guest cancellation is unimplemented; byte-at-a-time vsock reads
won't scale (#34). Operational flake: `petri sandbox create` intermittently
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
Near-term focus: with both capability axes now in place, harden the
host↔guest dispatch path against the known reliability bugs (#32/#33/#34)
before broadening surface (SDKs/backends). #32 (dispatch error recovery)
and #33 (state-file locking) are now done; #34 (vsock read scaling) is the
last of the named dispatch-reliability bugs. After #34 the path is clear to
broaden surface (SDKs/backends).

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

## Next Actions
1. #34 — byte-at-a-time vsock reads won't scale in the Swift helper. Now the
   top priority: the last named dispatch-reliability bug, and the gate before
   broadening surface (SDKs/backends).
2. #35 — low-severity code-review cleanups (polish; batch opportunistically).
3. Re-diagnose the `sandbox create` hang if it recurs. Its historical links
   (#32/#33) are both fixed, so it can no longer be attributed to them; needs
   a fresh root-cause pass (likely in the Swift helper boot/ready path).
4. #23 — runtime full-policy replacement / `setPolicy` (`scope: deferred`):
   the residual beyond the two capability axes; needs the
   narrow-only-vs-redefine-ceiling design call before implementation.
