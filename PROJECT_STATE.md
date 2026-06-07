# PROJECT STATE
_Last updated: 2026-06-07 — #36 network axis re-scoped to in-guest nftables + privilege drop_

## Current State
Petri is an early-stage microVM sandbox for running untrusted agent
workloads. It comprises a host `petri` CLI/lib, a `petri-guest` agent
(policy enforcement + vsock NDJSON dispatch), and a `petri-vz` Swift
helper driving Apple Virtualization. Working today: VM lifecycle state
machine, workspace mounting, shared protocol schema, LSP-backed semantic
tools in the guest, network policy enforced at the VM boot boundary, and
the new command-mode capability axis (none<read_only<edit<yolo) with
runtime `set_mode` escalation bounded by a boot-declared ceiling.
Only the macOS/Apple Virtualization backend exists. Known incomplete:
network is a single boot on/off with no runtime egress filtering yet;
guest cancellation is unimplemented; lifecycle state file has no locking
(#33); a transient dispatch error can brick an instance (#32);
byte-at-a-time vsock reads won't scale (#34).

## Active Direction
An E2B-style sandbox you run on your own hardware (local or self-hosted
server), with a hardened policy model — boot-declared capability ceilings
and runtime mode switching — and eventually a remote HTTP control plane.
Near-term focus: solidify the host↔guest dispatch path and policy model
on the macOS backend.

## Known Deviations
1. #31 was resolved more strongly than its `documentation` label implied:
   instead of documenting the shell-allowlist footgun, we built a command
   capability axis with runtime escalation (ADR 0002).
2. Network axis intentionally split — `network_enabled` stays the immutable
   boot gate; runtime egress filtering deferred to the network axis (#36).
   Partially delivers #23 (runtime policy updates).
3. #36 enforcement pivoted host-side → in-guest. The host-side spike (smoltcp
   over a VZFileHandle attachment) worked but ran ~130× slower than Apple NAT
   (~350 Mbit/s vs ~46 Gbit/s; spike doc 0036), so the design moved to in-guest
   nftables + unprivileged tool execution. ADR 0002 revised accordingly; the
   spike code is parked in `git stash` ("spike/0036 smoltcp ...").

## Next Actions
1. #36 — network axis (in-guest), remaining slices. DONE + VM-VERIFIED: per-child
   privilege drop (`setgroups([])`→gid→uid, guarded on euid==0 and the new
   `drop_privileges` boot-policy field), `agent` user (uid/gid 1000) + userns
   sysctl in the image. Booted privdrop-test image: tools run `uid=1000(agent)`,
   no leaked root supplementary group, workspace writable; builder runs as root
   via `drop_privileges = false`. (The VM test surfaced that the unconditional
   drop broke the image builder — fixed by making it boot-policy controlled.)
   TODO: document the `drop_privileges` field (done in policy-config + ADR);
   `[policy.network]` parsing; nftables ruleset (named set, `ct state new` gate)
   applied at boot and on `set_mode`; wire the `network` field on `set_mode`
   (guest rejects it today); then the DNS-proxy domain layer. See ADR 0002.
2. #32 — transient dispatch error permanently bricks an instance (bug).
3. #33 — lifecycle state file locking; concurrent dispatch races (bug).
4. #34 — byte-at-a-time vsock reads won't scale in the Swift helper.
5. #23 — runtime full-policy replacement / `setPolicy` (`scope: deferred`):
   re-scoped to the residual beyond the command axis; needs the
   narrow-only-vs-redefine-ceiling design call before implementation.
