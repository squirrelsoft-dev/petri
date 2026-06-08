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

Only the macOS/Apple Virtualization backend exists. Known incomplete /
broken: guest cancellation is unimplemented; a transient dispatch error can
brick an instance (#32); the lifecycle state file has no locking (#33);
byte-at-a-time vsock reads won't scale (#34). Operational flake: `petri
sandbox create` intermittently hangs (~30 min, 0 CPU, no instance dir) —
workaround is `pkill -9 -f "sandbox create"; pkill -9 -f petri-vz` and retry
(related to #32/#33). Network domain filtering is good-faith, not a hard
per-domain guarantee (shared-CDN-IP and DoH bypasses remain; ADR 0002).

## Active Direction
An E2B-style sandbox you run on your own hardware (local or self-hosted
server), with a hardened policy model — boot-declared capability ceilings
and runtime mode switching — and eventually a remote HTTP control plane.
Near-term focus: with both capability axes now in place, harden the
host↔guest dispatch path against the known reliability bugs (#32/#33/#34)
before broadening surface (SDKs/backends).

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
3. Discovered + fixed during #36 verification: sandbox images never brought up
   the network interface, so in-guest egress was non-functional out of the box
   on any prior image. Fixed by baking a systemd-networkd DHCP config into the
   base image (`14c9c78`); shipped base image rebuilt (`5a7055e`). Rebuild also
   fixed a chain of latent base-image build bugs (chroot mounts/env, TOML-array
   comment parsing, 2G→8G disk).

## Next Actions
1. #32 — transient dispatch error permanently bricks an instance (bug). Top
   priority: it's the most disruptive reliability defect and underlies the
   `sandbox create` hang seen repeatedly this loop.
2. #33 — lifecycle state file locking; concurrent dispatch races (bug).
3. #34 — byte-at-a-time vsock reads won't scale in the Swift helper.
4. #35 — low-severity code-review cleanups (polish; batch opportunistically).
5. #23 — runtime full-policy replacement / `setPolicy` (`scope: deferred`):
   the residual beyond the two capability axes; needs the
   narrow-only-vs-redefine-ceiling design call before implementation.
