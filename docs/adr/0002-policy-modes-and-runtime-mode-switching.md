# ADR 0002: Policy Modes And Runtime Mode Switching

## Status

Accepted. **Revised 2026-06-07** (network-axis enforcement): the original
decision enforced the network axis *host-side*, at the VM boundary, on the
premise that any in-guest filter is "defeatable by a root workload." Spike #36
([`docs/spikes/0036-network-egress-filter.md`](../spikes/0036-network-egress-filter.md))
implemented that host-side datapath and proved it correct but ~130× slower than
Apple's in-framework NAT (~350 Mbit/s vs ~46 Gbit/s). We are therefore moving
network enforcement **in-guest** (nftables) and resolving the "root workload"
objection a different way — by running agent tools **unprivileged**, so no
workload (not even at `command = yolo`) ever holds `CAP_NET_ADMIN` to tamper
with the ruleset. The sections below have been updated; the original host-side
rationale is preserved inline where it informs the trade-off. The host-side
implementation is parked in `git stash` should a future
"survives-guest-root-compromise" high-assurance mode want it.

## Context

Petri's boot policy is a flat document: a single `allowed_commands` list, a single
`network_enabled` boolean, and fixed caps. Two problems follow from that shape.

First, the command allowlist has a silent footgun (issue #31). `Policy::allows_command()`
checks only the program name; `argv` is unrestricted. A policy with
`allowed_commands = ["bash"]` therefore permits `bash -lc '<anything>'`, which silently
nullifies the allowlist. This is exactly what the builder policy ships
(`write_builder_policy` emits `["bash"]`). The allowlist *looks* like a security control
but a single shell entry turns it into `*` with no signal to the policy author. Documenting
the sharp edge — the originally suggested fix — does not remove it.

Second, changing posture requires a full teardown -> edit policy -> reboot cycle, because
the policy is immutable for the VM lifetime and there is no way to move between a locked-down
and a permissive stance on a live VM. That is at odds with the project's goal: a microVM
framework that runs locally or on a user's own server so the user can use their own hardware
as a sandbox the way they would pay to use a cloud sandbox. In the cloud-sandbox ergonomic, a
caller spins up a VM and adjusts what it can do on the fly (E2B, for example, mutates network
egress on a running sandbox via `updateNetwork` with no restart). Requiring a reboot per
posture change makes local "shell in and iterate" workflows painful.

A planned future also raises the stakes: an HTTP control endpoint (in addition to vsock) so a
remote agent can connect to a VM running on the user's Linux server and test dangerous code in
isolation on different hardware. vsock is local-only by physics, so "the host caller is
trusted" is currently free. An HTTP endpoint makes the control plane remotely reachable, so
whatever bounds a caller's authority must be explicit and enforced, not implied by the
transport.

The naive answer — let `set_mode` switch to an `allowlist = *` state on request — collides with
Petri's documented invariant that the host cannot widen authority after boot
(`policy-config.md`, `sandbox-safety-model.md`). This ADR reconciles "no reboot to change
posture" with that invariant.

## Decision

Replace the flat allowlist/network fields with a **capability lattice**. The boot policy
declares, per axis, an ordered set of levels, a starting level, and a **maximum level
(escalation ceiling)**. A new `set_mode` control frame lets the trusted host caller move the
VM's active level up or down *within* the boot-declared ceiling, on a live VM, with no reboot.
The guest enforces the ceiling and rejects any request above it.

The ceiling is fixed at boot and immutable. The boot policy is therefore still the maximum
authority available in the VM — it is just expressed as a ceiling the caller may climb to,
rather than a single fixed point. "The host cannot widen authority beyond boot policy" still
holds: `set_mode` can never exceed the per-axis `max`.

### Capability Axes

Authority is modeled as independent ordered axes rather than a single named ladder, so callers
can compose a point in the lattice (for example, "edit commands, no network") without the policy
author having to pre-name every combination. The first two axes:

**`command`** — what processes the `bash_command` tool may launch.

| Level | Meaning |
|---|---|
| `none` | Process execution disabled; `bash_command` is always rejected. |
| `read_only` | Only commands in the policy's `read_only` set may launch. Intended for non-mutating inspection. |
| `edit` | Only commands in the policy's `edit` set may launch. Intended for inspection plus file mutation and build tools. |
| `yolo` | Any executable may launch, equivalent to `*`, including arbitrary shells and `argv`. |

`read_only` and `edit` are policy-provided named allowlists; Petri enforces the active level's
ceiling, the policy author curates each level's contents. `yolo` needs no list. Crucially, a
shell only reaches `*`-equivalent power at the `yolo` level, which is honestly named — the
issue #31 footgun (a shell smuggled into a curated list silently meaning `*`) is gone because
"allow everything" is now an explicit, boot-declared level, not an accident of curation.

**`network`** — outbound egress available to guest processes. The axis is a switchable
**in-guest egress filter (nftables)** layered on top of the immutable boot gate:

- `network_enabled` (boot, immutable) decides whether a network device is attached to the VM
  at all. `network_enabled = false` attaches no device, so the axis is pinned at `none` and the
  only path to any network is teardown, reconfigure, and restart.
- When `network_enabled = true` a device is attached (Apple's in-framework
  `VZNATNetworkDeviceAttachment` — full-speed NAT, host-invisible), and the axis level selects
  the nftables ruleset `petri-guest` applies inside the guest:

| Level | Meaning (requires `network_enabled = true`) |
|---|---|
| `none` | Device attached but all outbound egress dropped by the in-guest ruleset. |
| `allowlist` | Egress only to the policy's listed IPs and CIDR blocks (an nftables named set). |
| `full` | Outbound filtering off; unrestricted egress. |

Because the device is Apple's in-framework NAT, in-guest DNS/UDP resolve normally; the
host-side spike's IP-literal limitation is gone. nftables matches IPs, not names, so domain
entries in the allowlist (`*.crates.io`) are enforced by a DNS proxy that populates the nft set
— see [Domain Allowlisting](#domain-allowlisting).

The axes are independent: `(command = edit, network = none)` and
`(command = read_only, network = full)` are both reachable if each axis's ceiling admits the
level. The escalation ceiling is per axis — capping `command` at `edit` and `network` at `full`
means `yolo` is forever unreachable on that VM, while either axis may still move freely below
its own cap.

### Enforcement Layers

**Both axes are guest-enforced**, by `petri-guest` running as root. This is a unification, not
a coincidence: the command level is checked where workload processes are launched, and the
network ruleset is applied where the network device lives — both inside the guest. A single
`set_mode` dispatch frame therefore governs both axes.

- **`command`** — the guest holds the active command level and checks it on every
  `bash_command`. Switching it means telling the guest, via `set_mode`.
- **`network`** — `petri-guest` applies an nftables ruleset for the active level. Switching it
  is an `nft` reconfiguration the guest performs on receipt of `set_mode` (for `allowlist`,
  swapping the contents of a named set; for `none`/`full`, swapping the default policy). The
  device itself is Apple's in-framework NAT and never changes.

**Why in-guest, given the threat model.** The original decision enforced network host-side on
the reasoning that an in-guest filter "would be defeatable by a root workload." Two things
resolve that objection:

1. **The workload never runs as root.** Agent tools are spawned as an unprivileged user
   (uid/gid 1000, no capabilities — see [Privilege Separation](#privilege-separation)). Even at
   `command = yolo`, "any executable may launch" means *as the unprivileged agent user*, not as
   root. Without `CAP_NET_ADMIN` the workload cannot load, flush, or edit nftables at all.
   `yolo` widens *what* runs, never *with what privilege*.
2. **Network is now exactly as strong as the command axis, not weaker.** The command axis was
   already guest-enforced — if a workload escaped to guest-root (kernel exploit, a bug in root
   `petri-guest` itself), it would already bypass the command ceiling regardless of where the
   network filter sat. Holding network to a uniquely higher "survives full guest-root
   compromise" bar made it the lone outlier. Co-locating it with the command axis makes the
   policy model coherent: the VM boundary protects the *host*; the in-guest enforcement protects
   *policy* against the untrusted workload, under the same assumption (guest privilege
   separation holds) that the rest of the policy model already relies on.

**The residual risk, stated plainly.** In-guest enforcement does *not* survive a guest-root
compromise. An attacker who gains root inside the guest can `nft flush ruleset`. The mitigations
above shrink the path to guest-root (unprivileged workload + `NoNewPrivileges` + no unprivileged
user namespaces — see below), but a kernel or `petri-guest` 0-day defeats it. This is an
accepted, documented limitation, identical in kind to the command axis. A
"survives-guest-root-compromise" network guarantee is a *separate, optional* feature: the
host-side boundary filter from spike #36 (parked in `git stash`), which can return as a
high-assurance mode for callers who need it and can pay the throughput cost.

Consequences for the network axis (implemented — IP/CIDR via nftables, domain names via the in-guest DNS proxy):

- `petri-guest` gains nftables application at boot (from policy) and on `set_mode`, using a
  **named set** for the allowlist so runtime updates are atomic (`nft add/delete element`) with
  no VM restart and no flow interruption.
- The guest still enforces the **boot ceiling**: a `set_mode` requesting `full` is clamped to —
  in fact rejected above — the boot-declared `network.max` before any `nft` change, the same
  way the command level is bounded.
- `iproute2` + `nftables` must be present in the guest image; the ruleset is applied before the
  vsock listener begins dispatching, so no workload runs before egress policy is in force.

### Privilege Separation

In-guest network enforcement is only sound if the untrusted workload cannot reach the ruleset.
That is achieved by privilege separation, which is also independently valuable (it confines what
a tool can touch on the guest filesystem and process table):

- **`petri-guest` stays root** for its own lifetime — it needs root to apply nftables, manage
  the vsock listener, and spawn children as another user. It does *not* drop its own privileges
  and does *not* restrict its own capability set (retaining e.g. `CAP_SYS_ADMIN` would leave it
  effectively root anyway; the confinement that matters is on the *children*, via uid, not on
  the parent's cap mask).
- **Each tool runs as an unprivileged user** (`agent`, uid/gid 1000). `petri-guest` drops
  privilege per child, between `fork` and `exec`, not on itself. The child inherits no
  capabilities because it is non-root and the guest carries `NoNewPrivileges=yes`.
- **Supplementary groups must be cleared explicitly.** Rust's `CommandExt::uid()/gid()` (and a
  bare `setuid`/`setgid`) do **not** call `setgroups()`, so a naive drop leaves the child
  holding root's supplementary groups (group 0, …). The spawn path must `setgroups(&[])`
  *before* the gid/uid switch (order: groups → gid → uid) — done in a `pre_exec` hook. This is
  the one easy-to-miss correctness bug and is called out so the implementation does not repeat
  it.
- **Re-escalation paths are closed:** `NoNewPrivileges=yes` (already set on the
  `petri-guest.service` unit) neuters setuid-root binaries for all descendants, so a dropped
  child cannot climb back via `sudo`/`su`/`mount`. Unprivileged user namespaces — the remaining
  realistic route to capabilities for a uid-1000 process — are disabled via sysctl
  (`kernel.unprivileged_userns_clone=0` / `user.max_user_namespaces=0`) at boot. A seccomp
  syscall filter on children was considered and **deferred**: agent tools are arbitrary
  (compilers, interpreters, package managers), so a sound allowlist is high-effort and
  high-breakage, and the userns sysctl closes the main escalation vector it would have targeted.
- **The policy file is root-only.** `/run/petri/policy.toml` (and the LSP config) must not be
  readable or writable by uid 1000, so a workload can neither read the ceiling nor tamper with
  it.
- **Workspace ownership.** `/workspace` is owned by `agent:agent` so tools can read/write it.
  The Apple virtio-fs share maps ownership from the host side, so this must be verified
  end-to-end — it is the most likely integration snag.

The per-request `cwd`/`env` contract from the existing dispatch path is preserved; privilege
drop is additive to it, not a replacement that hardcodes a working directory.

The drop is governed by a boot-policy field, `drop_privileges`, defaulting to `true` (secure).
Trusted provisioning contexts that legitimately need root command execution — the image
builder, whose dispatched commands write `/etc`, install packages, and run `mmdebstrap` — set
`drop_privileges = false`. This is a boot-time, immutable choice by the trusted host, consistent
with the rest of the policy model; it is *not* a runtime escape hatch and cannot be changed via
`set_mode`. (This field exists because an unconditional drop was found to break the image
builder, which reuses `petri-guest` as a root command executor.)

### Domain Allowlisting

The `allowlist` network level admits both IPs/CIDRs (matched directly by nftables) and **domain
names** (`*.crates.io`, `api.github.com`). nftables cannot match names, so domains are enforced
by a **DNS proxy that `petri-guest` runs and the guest is forced to use**, which resolves allowed
names and dynamically populates the nft set with the resulting IPs:

```
tool resolves api.github.com
  └── query → petri-guest DNS proxy (127.0.0.1:53)
        ├── name matches allowlist → forward to upstream (e.g. 1.1.1.1),
        │     add each returned A/AAAA to the nft set with timeout = record TTL,
        │     return the answer  → subsequent connect() to that IP is accepted
        └── name not allowed → NXDOMAIN (no IP, no nft entry)
```

IPs/CIDRs stay enforced directly by nftables; the proxy is the enforcement point for *names*.
Together they cover the full allowlist surface. hickory-dns (pure-Rust resolver/server) is the
intended implementation.

**Mechanics that make it sound (hard requirements, not options):**

1. **All DNS is forced through the proxy.** `/etc/resolv.conf` → `127.0.0.1` is only a
   convention; a workload can query an upstream directly. nftables must therefore **drop all
   egress to ports 53 and 853** except to the local proxy, so name resolution cannot route
   around it.
2. **The nft allowlist set is checked only for new connections.** The ruleset accepts
   `ct state established,related` above the set lookup and gates `ct state new` on the set. An
   IP entry expiring then blocks only *future* connections to it — long-lived flows are never
   cut mid-transfer. This is what makes TTL-based expiry safe.
3. **Per-element timeouts, set to the record TTL.** Use nftables' native set `timeout` per
   element rather than a cleanup loop; the set self-cleans as DNS answers age out. Add every
   A/AAAA record returned (round-robin), with a parallel `ip6` set for AAAA (or disable guest
   IPv6 to shrink the surface).

**Accepted residual limits (this is good-faith domain filtering, not a hard per-domain
guarantee), to be stated in the safety docs:**

- **IP granularity can't separate vhosts on a shared IP.** When an allowed domain and a denied
  one sit behind the same CDN/shared-hosting IP (common with Cloudflare/Fastly), allowing the
  first opens the IP, and a workload can then reach the second on that IP via a forged
  `Host`/SNI. nftables sees only the IP. Likewise, any allowlisted domain whose DNS the workload
  can influence can be pointed at an arbitrary IP, which the proxy will then admit.
- **DoH bypass.** DNS-over-HTTPS is indistinguishable from normal port-443 traffic, so a
  workload can resolve names out-of-band via any already-allowed IP that also serves DoH (public
  resolvers ride the same CDNs as content). nftables cannot close this with IP filtering alone;
  not allowlisting known DoH endpoints narrows it but does not eliminate it.
- **Same trust basis as the rest of the model.** Both the proxy and the nft ruleset are in-guest
  and rest on privilege separation; a guest-root compromise defeats them, exactly as it defeats
  the command axis.

A *hard* per-domain guarantee (separating vhosts on shared IPs, defeating domain fronting)
requires **L7 egress mediation** — an SNI-inspecting transparent proxy, a forward/`CONNECT`
proxy the tools are pointed at, or a MITM proxy with a guest-trusted CA — which is a separate,
larger effort filed alongside the stashed host-side filter as a future high-assurance option,
not part of this work.

### Boot Policy Schema

The schema below shows both axes. `[policy.command]` and `[policy.network]` are both
implemented; the network axis enforces IP/CIDR allowlist entries via nftables at boot and on
`set_mode`, and **domain-name** entries via the in-guest DNS proxy (see below). A
`[policy.network]` block subsumes the legacy boolean `network_enabled` (an attached device with
a ceiling); with no block present the axis derives from `network_enabled`
(`true` → full egress, `false` → none).

```toml
[policy]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"

[policy.command]
default = "read_only"          # active level at boot
max = "yolo"                   # escalation ceiling for this axis
read_only = ["ls", "cat", "grep", "rg", "find"]
edit = ["ls", "cat", "grep", "rg", "find", "sed", "tee", "cargo", "git"]
# yolo needs no list; it means *

# Guest-enforced: IP/CIDR via nftables, domain names via the in-guest DNS proxy.
[policy.network]
default = "none"
max = "allowlist"              # full egress is never reachable on this VM
allowlist = ["1.1.1.1", "8.8.8.0/24", "*.crates.io"]
```

In the example above, a caller may climb `command` all the way to `yolo`, but `network` can
never exceed `allowlist`. `default` must be `<= max` on each axis.

### `set_mode` Control Frame

`set_mode` is a host control frame (it uses the existing `control` field, alongside `cancel`).
Because both axes are guest-enforced (see [Enforcement Layers](#enforcement-layers)), the frame
carries either or both axis levels; omitted axes are left unchanged:

```json
{
  "protocol_version": 1,
  "id": "req-9",
  "control": "set_mode",
  "args": { "command": "edit", "network": "allowlist" }
}
```

Guest behavior:

- Reject (`policy_denied`) if a requested level exceeds that axis's `max`.
- Reject (`invalid_request`) if a level name is unknown for its axis.
- Reject (`invalid_request`) if neither `command` nor `network` is present.
- For `command`: set the VM's active command level; it applies to subsequent dispatches.
- For `network`: apply the corresponding nftables ruleset (swap the allowlist set's contents, or
  the default policy). The frame succeeds only if the `nft` application succeeds; on `nft`
  failure the prior ruleset stays in force and the frame returns an error rather than leaving
  egress in an undefined state.
- Return a success result frame echoing the new level(s). Both axes change atomically from the
  caller's view: validate all requested axes against their ceilings first, then apply.

Movement is bidirectional within the ceiling, per axis: a caller may climb (`read_only` ->
`edit` -> `yolo`, or `none` -> `allowlist` -> `full`) or drop back down. Dropping down is always
allowed; climbing is allowed only up to that axis's `max`.

A host control surface (the local CLI/API today, a remote HTTP endpoint later) exposes a unified
mode change and routes both axes to the guest in this single frame. The host clamps each
requested level to the boot ceiling before sending as defense in depth, but the guest is the
authoritative enforcer of both.

### Trust Model Clarification

Petri's existing invariant — "the host cannot widen authority after boot" — actually protects
against **guest workload escalation**. This ADR makes the distinction explicit:

- **Guest workload escalating its own authority** — forbidden, unchanged. The untrusted
  workload runs as launched processes; it cannot emit dispatch or control frames over vsock.
  It can never issue `set_mode`.
- **The trusted host caller transitioning between boot-declared levels** — permitted. This is a
  control-plane action by the trusted principal, bounded by the immutable per-axis ceiling.

The two are different actors. `set_mode` changes nothing about the guest workload's inability
to escalate; it only lets the trusted caller move the VM within authority the boot policy
already granted.

### Transport Independence And The Future HTTP Plane

`set_mode` is a protocol frame, not a vsock-specific mechanism. When an HTTP control endpoint is
added, the per-axis ceiling becomes the defense-in-depth backstop that bounds what *any*
connection to the VM can escalate to, independent of endpoint authentication. Endpoint auth
governs *who may connect*; the ceiling governs *what a connection may ever reach*. Both are
required: auth can have bugs, and the ceiling is the bound that survives them. The server owner
sets each VM's ceiling at launch — a throwaway dangerous-code VM may be capped at `yolo`, a
longer-lived VM capped at `edit` / `allowlist` — without changing the control-plane code.

## Scope

First cut implements the **`command` axis** end to end: schema, `set_mode`, guest enforcement,
and an updated builder policy that declares an explicit `yolo` level instead of smuggling `*`
via `["bash"]`. This is the direct fix for issue #31.

The **`network` axis** was specified here and has since landed (issue #36). Like `command`, it
is **guest-enforced** via the same `set_mode` frame (see [Enforcement Layers](#enforcement-layers));
spike #36 ruled out the originally-planned host-side filter on throughput grounds. A policy with
no `[policy.network]` block still degrades to the single boot-time `network_enabled` gate with no
escalation: `true` means full egress for the VM's lifetime, `false` means none. The axis adds,
concretely:

- An `agent` user (uid/gid 1000) in the guest image, with `/workspace` owned by it, and the
  per-child privilege drop (`setgroups([])` → gid → uid in `pre_exec`) in `execute_command`.
- Boot-time hardening: the `kernel.unprivileged_userns_clone` / `user.max_user_namespaces`
  sysctl, and root-only perms on `/run/petri/policy.toml`.
- `petri-guest` applying the nftables ruleset at boot (from `[policy.network]`) and on
  `set_mode`, using a named set for the allowlist (`ct state new` gate, established always
  accepted) plus a parallel `ip6` set; `iproute2` + `nftables` added to the image.
- A `petri-guest`-run DNS proxy (hickory-dns) for domain entries, with egress to ports 53/853
  dropped except to it, and per-element nft set timeouts set to record TTLs
  (see [Domain Allowlisting](#domain-allowlisting)). Domain support can land as a second step
  after IP/CIDR allowlisting works.
- The `network` field on the `set_mode` frame and its result, with ceiling validation.

Proving the mode model on the guest-enforced `command` axis first still holds — the network
axis simply reuses the same enforcement layer rather than introducing a host-side one.

### Backward Compatibility

The existing flat schema maps onto a degenerate lattice with no escalation room, so current
policies keep working:

- `allowed_commands = [...]` becomes a single `command` axis where `default == max` and both
  point at a policy-provided set; the VM cannot escalate or de-escalate.
- `network_enabled` maps to a `network` axis pinned at `none` or `full` with `default == max`.

A policy that declares no `max` above its `default` is exactly today's immutable single-mode VM.

## Consequences

- The allowlist stops being a misleading control: `*`-equivalent authority is reachable only
  through an explicitly named `yolo` level, declared at boot, never through curation accidents.
- Callers adjust posture on a live VM with no teardown/reboot, matching the local and
  self-hosted-server ergonomics the project targets.
- The boot policy gains real expressiveness (a ceiling and a starting point per axis) at the
  cost of a larger schema and a new control frame the guest must validate carefully. `set_mode`
  validation joins path canonicalization and process launching as security-critical guest code.
- The active mode (both axes) is per-VM mutable state in the guest. It must default to the boot
  `default`, must never exceed `max`, and must be set only by control frames (never inferable
  from a workload process), or the escalation ceiling is meaningless.
- Network enforcement gains its safety from privilege separation, not from the VM boundary. This
  is a deliberate, documented step down from "survives guest-root compromise" to "as strong as
  the command axis" — bought in exchange for native-NAT throughput and a far smaller surface (no
  userspace netstack to maintain). The high-assurance host-side filter remains available
  (stashed) if a caller ever needs the stronger guarantee.
- Privilege separation is now security-critical guest code on the same footing as `set_mode`
  validation: the `setgroups([])` → gid → uid ordering, the userns sysctl, and root-only policy
  perms each individually gate the network guarantee. A regression in any one re-opens the
  tamper path the host-side design closed structurally.
- Documentation must be updated to match, since the current wording bans `set_mode` outright and
  describes network as host-enforced:
  - `policy-config.md` — the lattice schema, per-axis `default`/`max`, and reworded immutability
    section distinguishing guest escalation (forbidden) from host-driven transitions within the
    ceiling (allowed).
  - `sandbox-safety-model.md` — the command and network sections, the immutability wording, the
    privilege-separation basis for the in-guest network guarantee (plus its residual guest-root
    caveat), and the domain-allowlist residual limits (shared-IP vhosts, DoH bypass).
  - `vsock-dispatch-protocol.md` — the `set_mode` control frame (now both axes) and its result.
  - `schema/petri-protocol-v1.schema.json` and `schema/fixtures/` — the new policy shape and a
    `set_mode` request/result fixture pair carrying both axes.

## References

- Issue #31: "Allowlisting a shell silently defeats the command allowlist"
- Issue #36: "Runtime-mutable network egress filter"
- [Spike #36: host-enforced egress filter](../spikes/0036-network-egress-filter.md) — the
  throughput evidence behind the in-guest pivot (host-side ~350 Mbit/s vs NAT ~46 Gbit/s)
- [ADR 0001: Petri Architecture](0001-petri-architecture.md)
- [Immutable Policy Config](../policy-config.md)
- [Sandbox Safety Model](../sandbox-safety-model.md)
- [Vsock Dispatch Protocol](../vsock-dispatch-protocol.md)
