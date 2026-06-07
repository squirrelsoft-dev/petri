# ADR 0002: Policy Modes And Runtime Mode Switching

## Status

Accepted

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
**egress filter over an attached network device**, layered on top of the immutable boot gate:

- `network_enabled` (boot, immutable) decides whether a network device is attached to the VM
  at all. `network_enabled = false` attaches no device, so the axis is pinned at `none` and the
  only path to any network is teardown, reconfigure, and restart.
- When `network_enabled = true` a device is attached, and the axis level selects the egress
  filter applied at the VM boundary:

| Level | Meaning (requires `network_enabled = true`) |
|---|---|
| `none` | Device attached but all outbound egress blocked by the boundary filter. |
| `allowlist` | Egress only to the policy's listed IPs, CIDR blocks, and domains. |
| `full` | Outbound blocking off; unrestricted egress. |

The axes are independent: `(command = edit, network = none)` and
`(command = read_only, network = full)` are both reachable if each axis's ceiling admits the
level. The escalation ceiling is per axis — capping `command` at `edit` and `network` at `full`
means `yolo` is forever unreachable on that VM, while either axis may still move freely below
its own cap.

### Enforcement Layers

The two axes are enforced at different layers, which determines how each is switched:

- **`command` is guest-enforced.** The guest launches workload processes, so it holds the
  active command level and checks it on every `bash_command`. Switching it requires telling the
  guest — that is what the `set_mode` dispatch frame is for.
- **`network` is host-enforced at the VM boundary.** Because the host issues the mode change
  and the host owns the network device and its egress filter, a network mode change is a
  host-local reconfiguration of that filter (nftables on the tap, pf/vmnet, etc.). The guest is
  never told and holds no network state. This is deliberate: the filter sits outside the VM, so
  no in-guest privilege — not even a `command = yolo` root workload — can tamper with it. A
  guest-side firewall would be defeatable by a root workload and would make the network
  guarantee weaker than the rest of Petri, contradicting the threat model
  (`sandbox-safety-model.md`: safety must not depend on the workload behaving). Therefore the
  guest-bound `set_mode` frame governs the **command axis only**; a `network` field in a
  guest-bound frame is rejected by design, not as a temporary stub.

Consequences for the network axis (follow-up work):

- The host-side egress filter must be **live-mutable without a VM restart** — a per-backend
  capability (Linux/Firecracker: nftables on the tap device; macOS/vz: pf or vmnet, pending
  confirmation that runtime mutation is supported).
- The host still enforces the **boot ceiling**: even when a future remote HTTP caller requests
  `full`, the trusted host process clamps the requested level to what the boot policy
  authorized before touching the filter. Same envelope, enforced host-side.

### Boot Policy Schema

The target schema below shows both axes. `[policy.command]` is implemented in the first cut;
`[policy.network]` is the follow-up form. Today the network gate is the existing boolean
`network_enabled` (a `[policy.network]` block subsumes it: an attached device with a ceiling,
or no block / `max = "none"` meaning no device).

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

# Follow-up (host-enforced). Today this is `network_enabled = true|false`.
[policy.network]
default = "none"
max = "allowlist"              # full egress is never reachable on this VM
allowlist = ["1.1.1.1", "8.8.8.0/24", "*.crates.io"]
```

In the example above, a caller may climb `command` all the way to `yolo`, but `network` can
never exceed `allowlist`. `default` must be `<= max` on each axis.

### `set_mode` Control Frame

`set_mode` is a host control frame (it uses the existing `control` field, alongside `cancel`).
Because only the **command** axis is guest-enforced (see [Enforcement Layers](#enforcement-layers)),
the guest-bound frame carries the command level only:

```json
{
  "protocol_version": 1,
  "id": "req-9",
  "control": "set_mode",
  "args": { "command": "edit" }
}
```

Guest behavior:

- Reject (`policy_denied`) if the requested command level exceeds the command axis `max`.
- Reject (`invalid_request`) if the command level name is unknown.
- Reject (`invalid_request`) a `network` field — the network axis is enforced host-side and is
  never carried in a guest-bound frame.
- Otherwise set the VM's active command level and return a success result frame echoing the new
  level. The change applies to subsequent dispatches.

Movement is bidirectional within the ceiling: a caller may climb (`read_only` -> `edit` ->
`yolo`) or drop back down (`yolo` -> `read_only`). Dropping down is always allowed; climbing is
allowed only up to `max`.

A host control surface (the local CLI/API today, a remote HTTP endpoint later) may expose a
unified mode change covering both axes. The host routes the command level to the guest as the
frame above, and applies the network level itself by reconfiguring the boundary egress filter.
The network change never reaches the guest.

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

The **`network` axis** is specified here but lands in a follow-up, and — unlike `command` — it
is **host-enforced**, not a guest dispatch frame (see [Enforcement Layers](#enforcement-layers)).
Until then, `network_enabled` remains a single boot-time gate with no escalation: `true` means
full egress for the VM's lifetime, `false` means none. The follow-up adds the host-side,
live-mutable boundary egress filter and the host-held network ceiling. Keeping the higher-blast-radius
axis boot-fixed for now lets the mode model be proven on the lower-risk, guest-enforced command
axis first.

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
- The active mode is per-VM mutable state in the guest. It must default to the boot `default`,
  must never exceed `max`, and must be set only by control frames (never inferable from a
  workload process), or the escalation ceiling is meaningless.
- Documentation must be updated to match, since the current wording bans `set_mode` outright:
  - `policy-config.md` — the lattice schema, per-axis `default`/`max`, and reworded immutability
    section distinguishing guest escalation (forbidden) from host-driven transitions within the
    ceiling (allowed).
  - `sandbox-safety-model.md` — the command and network sections, and the immutability wording.
  - `vsock-dispatch-protocol.md` — the `set_mode` control frame and its result.
  - `schema/petri-protocol-v1.schema.json` and `schema/fixtures/` — the new policy shape and a
    `set_mode` request/result fixture pair.

## References

- Issue #31: "Allowlisting a shell silently defeats the command allowlist"
- [ADR 0001: Petri Architecture](0001-petri-architecture.md)
- [Immutable Policy Config](../policy-config.md)
- [Sandbox Safety Model](../sandbox-safety-model.md)
- [Vsock Dispatch Protocol](../vsock-dispatch-protocol.md)
