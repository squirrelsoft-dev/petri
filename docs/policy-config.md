# Immutable Policy Config

Each Petri VM boots with a TOML policy config that defines what the guest agent is allowed to do. The policy is loaded before the agent accepts dispatch requests and is immutable for the lifetime of the VM.

The policy is the maximum authority available inside the VM. A dispatch request may narrow limits for a single request, but it cannot attach a network device that boot policy left off, permit new commands, raise caps, or change workspace boundaries after boot.

Authority is expressed as ordered **capability axes**, each with a boot-default level and an escalation **ceiling**. A live VM may move an axis's active level up to its ceiling via the `set_mode` control frame — without a reboot — but never past it. The ceiling, not the starting level, is the immutable maximum. This is the mechanism described in [ADR 0002](adr/0002-policy-modes-and-runtime-mode-switching.md); see [Command Modes](#command-modes) and [Network Modes](#network-modes) below.

> **Implementation status.** The `command` axis is implemented. The `network` axis is the target shape per ADR 0002 and lands in a follow-up; today network is the single immutable `network_enabled` boolean gate (no runtime escalation). The `[policy.network]` block and the network half of `set_mode` are documented here ahead of that work.

## Policy Templates

Rather than hand-writing a TOML file each time, reusable policies can be managed as named **templates** with the `petri policy` subcommands. Built-in templates ship with the binary; user templates live as `<name>.toml` files under `~/.petri/policies/` (overridable via `PETRI_POLICIES_DIR`). A user template whose name matches a built-in *shadows* it.

| Built-in | Posture |
|---|---|
| `locked-down` | No network; `command` pinned at `read_only` (no escalation). Untrusted inspection. |
| `developer` | No network; boots `read_only`, escalates to `edit` with common build tools. |
| `yolo` | Full egress and `command = yolo` (arbitrary shells). Trusted/throwaway only. |
| `fetch` | Network on; `read_only` → curated fetch tools (`git`, `curl`, `wget`); tight caps. |

```
petri policy list                              # built-ins + user templates, with posture
petri policy show developer                    # print a template's TOML (pipeable)
petri policy path developer                    # print its resolved on-disk path
petri policy create my-ci --from developer     # new user template (defaults to --from locked-down)
petri policy edit my-ci                         # open in $EDITOR; forks a built-in copy-on-write
petri policy remove my-ci                       # delete a user template
```

Built-in templates are never edited or deleted in place: `petri policy edit <builtin>` forks a user copy first, and `petri policy remove <builtin>` only ever removes a user override (restoring the built-in). Templates are validated against this schema on create/edit, so the registry never holds a policy that would fail at boot.

Anywhere `--policy` is accepted, you may pass a **template name** in place of a file path. An existing file always wins; otherwise a bare name resolves through the registry (user override first, then built-in):

```
petri sandbox create trixie --workspace . --policy developer     # by template name
petri sandbox create trixie --workspace . --policy ./custom.toml  # by file path
```

## Schema

```toml
[policy]
network_enabled = false
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
# drop_privileges = true   # default; see Privilege Separation below

[policy.command]
default = "read_only"
max = "edit"
read_only = ["ls", "cat", "rg", "find"]
edit = ["cargo", "rustc", "git", "sed", "tee"]

# Target shape (ADR 0002); today use the `network_enabled` boolean instead.
# Requires network_enabled = true to attach a device for the axis to filter.
[policy.network]
default = "none"
max = "allowlist"
allowlist = ["1.1.1.1", "8.8.8.0/24", "*.crates.io"]
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `network_enabled` | boolean | yes | Whether a network device is attached to the VM at all. `false` pins the network axis at `none` for the VM's lifetime. Defaults should be treated as deny if a future loader supports omitted fields. |
| `[policy.command]` | table | yes\* | Command capability axis. See [Command Modes](#command-modes). |
| `[policy.network]` | table | no | Network capability axis (target shape; see [Network Modes](#network-modes)). When present it requires `network_enabled = true`. When absent, network is governed solely by the `network_enabled` boolean. |
| `allowed_commands` | array of strings | yes\* | Legacy flat allowlist. Mutually exclusive with `[policy.command]`. Entries are command names, not shell snippets. |
| `max_runtime_secs` | positive integer | yes | Maximum wall-clock runtime for one dispatch. |
| `max_output_bytes` | positive integer | yes | Maximum combined stdout and stderr bytes returned for one dispatch. |
| `workspace_path` | absolute string path | yes | Canonical shared workspace root inside the guest. |
| `drop_privileges` | boolean | no | Whether the guest drops each workload process to the unprivileged `agent` user before exec. Defaults to `true` — the secure posture. See [Privilege Separation](#privilege-separation). |

\* Exactly one of `[policy.command]` or `allowed_commands` must be present. Setting both, or neither, is invalid.

The `[policy]` table is required. Unknown fields are invalid unless a future schema version explicitly defines them.

## Network Controls

`network_enabled` is the immutable boot gate. `network_enabled = false` attaches no network device, so the VM has no outbound network access for its lifetime — the network axis is pinned at `none` and the only way to any network is teardown and reboot with a new policy. This is the default safety posture and should be used for ordinary build, test, and inspection workloads.

`network_enabled = true` attaches a network device (Apple's in-framework NAT on the macOS backend). It does not bypass command allowlists, runtime caps, output caps, or workspace rules; the guest still rejects any dispatch that violates another policy field. With a device attached, the **network axis** selects how much of that device's egress is permitted.

### Network Modes

> Target shape (ADR 0002). Today, with no `[policy.network]` block, `network_enabled = true` means full egress for the VM's lifetime and `false` means none.

The `network` axis controls outbound egress and, like `command`, is **guest-enforced** — `petri-guest` applies an nftables ruleset inside the VM for the active level. It has three ordered levels:

| Level | Authority (requires `network_enabled = true`) |
|---|---|
| `none` | Device attached but all outbound egress dropped by the in-guest ruleset. |
| `allowlist` | Egress only to the policy's listed IPs, CIDR blocks, and domains. |
| `full` | Outbound filtering off; unrestricted egress. |

```toml
[policy.network]
default = "none"          # active level at boot
max = "allowlist"         # ceiling; `full` is unreachable on this VM
allowlist = ["1.1.1.1", "8.8.8.0/24", "*.crates.io"]
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `default` | level name | yes | Active level when the VM boots. Must be `<= max`. |
| `max` | level name | yes | Escalation ceiling. The active level can never exceed this, even via `set_mode`. |
| `allowlist` | array of strings | no | Allowed destinations for the `allowlist` level: IPv4/IPv6 addresses, CIDR blocks, and domain names (including `*.` wildcards). Defaults to empty (which makes `allowlist` equivalent to `none`). |

The active level moves at runtime with `set_mode`, bounded by `max` — see [Runtime Mode Switching](#runtime-mode-switching). Because enforcement is in-guest, agent tools run **unprivileged** (no `CAP_NET_ADMIN`) so a workload cannot edit the ruleset; this rests on guest privilege separation, the same basis as the command axis, and does **not** survive a guest-root compromise. See [ADR 0002 Enforcement Layers](adr/0002-policy-modes-and-runtime-mode-switching.md#enforcement-layers).

#### Domain Allowlisting

IPs and CIDRs are matched directly by nftables. **Domain** entries are enforced by a DNS proxy `petri-guest` runs: a query for an allowed name is resolved upstream and the resulting IPs are added to the nftables set (with a timeout equal to the record TTL); a query for a non-allowed name returns NXDOMAIN. All guest DNS is forced through this proxy (egress to ports 53/853 is dropped except to it).

This is **good-faith domain filtering, not a hard per-domain guarantee.** It cannot separate vhosts that share a CDN/hosting IP (allowing one domain opens the IP for any name on it via a forged Host/SNI), and it cannot stop DNS-over-HTTPS resolution that rides an already-allowed IP. A hard per-domain guarantee would require L7 egress mediation (SNI/CONNECT/MITM proxy), tracked separately. See [ADR 0002 Domain Allowlisting](adr/0002-policy-modes-and-runtime-mode-switching.md#domain-allowlisting).

## Command Modes

The `command` axis controls which executables the `bash_command` tool may launch. It has four ordered levels, each granting strictly more authority than the last:

| Level | Authority |
|---|---|
| `none` | Process execution disabled; every `bash_command` is rejected. |
| `read_only` | Only commands in the policy's `read_only` set may launch. Intended for non-mutating inspection. |
| `edit` | Only commands in the policy's `read_only` **and** `edit` sets may launch. Intended for inspection plus file mutation and build tools. |
| `yolo` | Any executable may launch, equivalent to `*`, including arbitrary shells and `argv`. |

Levels are cumulative: `edit` includes everything `read_only` allows. `yolo` needs no list.

```toml
[policy.command]
default = "read_only"     # active level at boot
max = "edit"              # escalation ceiling; yolo is unreachable on this VM
read_only = ["ls", "cat", "rg", "find"]
edit = ["cargo", "rustc", "git", "sed", "tee"]
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `default` | level name | yes | Active level when the VM boots. Must be `<= max`. |
| `max` | level name | yes | Escalation ceiling. The active level can never exceed this, even via `set_mode`. |
| `read_only` | array of strings | no | Executable names allowed at `read_only` and above. Defaults to empty. |
| `edit` | array of strings | no | Additional executable names allowed at `edit` and above. Defaults to empty. |

Command entries are executable names, not shell text. For example, `"cargo"` is valid, but `"cargo test"` is invalid. Arguments are evaluated as part of the dispatch request and do not grant permission to execute additional binaries. Shell metacharacters, path traversal, and command chaining are rejected as command names.

A curated level (`read_only` or `edit`) should not contain a shell such as `bash`, `sh`, or `python`. A shell entry in a curated level reintroduces arbitrary execution at that level: `bash` permits `bash -lc '<anything>'`, because policy matches the program name, not `argv`. When a workload genuinely needs a shell or unrestricted execution, declare a `yolo` level and switch to it deliberately, rather than smuggling `*` into a list that is presented as curated.

### Runtime Mode Switching

A running VM moves an axis's active level with the `set_mode` control frame, with no reboot. The frame carries the `command` axis, the `network` axis, or both. The trusted host caller may climb toward each axis's `max` or drop back down; the guest validates against each ceiling and rejects any request above `max` with `policy_denied`. The frame and its semantics are defined in [Vsock Dispatch Protocol](vsock-dispatch-protocol.md#mode-switching).

This is a control-plane action by the trusted host. The untrusted guest workload cannot emit dispatch or control frames, so it can never issue `set_mode` or otherwise escalate its own authority. See [Runtime Immutability](#runtime-immutability).

### Legacy Allowlist

A policy may instead set a flat `allowed_commands = [...]` array. This maps to a fixed `edit`-level axis with no escalation room (`default == max == edit`), preserving the original single-allowlist behavior. It is mutually exclusive with `[policy.command]`. An empty `allowed_commands` (or a `none` default with empty sets) means the guest rejects all process execution.

## Privilege Separation

`drop_privileges` controls whether the guest agent runs workload processes as an unprivileged user. It defaults to `true`: `petri-guest` stays root (it needs root to manage the listener and, in future, nftables) but drops each spawned tool to the `agent` user (uid/gid 1000) before exec, so no workload — even at `command = yolo` — holds capabilities or can touch privileged state. The drop only takes effect when the guest agent is running as root; in dev/test contexts where it is not, there is nothing to drop. The target image must provide the `agent` user (the base image build does).

Set `drop_privileges = false` only for **trusted provisioning** contexts whose commands must run as root — the image builder is the sole example today (it writes `/etc`, installs packages, runs `mmdebstrap`). Leaving it at the default for any sandbox that runs untrusted workloads is required for the safety model. See [ADR 0002](adr/0002-policy-modes-and-runtime-mode-switching.md#privilege-separation) and [Sandbox Safety Model](sandbox-safety-model.md#process-privilege-separation).

## Runtime And Output Caps

`max_runtime_secs` is the maximum wall-clock time allowed for one dispatch. The guest agent terminates work that exceeds this cap and returns a policy failure or timeout result.

`max_output_bytes` is the maximum combined stdout and stderr payload returned for one dispatch. Output beyond the cap is truncated or rejected according to the result protocol, but it must not be allowed to grow without bound.

Per-request limits may be lower than the boot policy values. They may not be higher.

## Workspace Path Rules

`workspace_path` must be an absolute path and must resolve to the configured shared workspace mount, normally `/workspace`.

The guest agent rejects dispatches that request a working directory outside `workspace_path`. Path checks must use canonicalized paths so relative segments and symlinks cannot escape the workspace root.

Petri exposes the workspace as the only shared filesystem surface unless a future policy schema explicitly adds another shared path model.

The host-side workspace contract requires an absolute existing host directory
and maps it into the guest at `/workspace`; see
[Workspace Mounting Contract](workspace-contract.md).

## Runtime Immutability

The guest agent loads the policy before accepting vsock dispatch requests. Once loaded, the policy — including each axis's escalation ceiling — remains fixed until the VM is torn down and a new VM boots with a new policy.

The rule that "authority cannot be widened after boot" protects against **guest workload escalation**. The untrusted workload runs as launched processes; it cannot emit dispatch or control frames over vsock, so it can never raise its own authority. This invariant is unchanged.

Two things are distinct and both allowed:

- **Request-scoped narrowing.** A dispatch may include narrower limits, such as a shorter runtime cap. The effective policy is always the most restrictive combination of the boot policy and request constraints. A request may not raise a cap above the boot policy.
- **Host-driven mode transitions.** The trusted host caller may move an axis's active level between boot-declared levels with `set_mode`, up to but never past that axis's `max`. This is a control-plane action by the trusted principal within authority the boot policy already granted — it does not widen the ceiling. The boot policy's per-axis `max` is the immutable maximum; `default` is merely where the VM starts.

The guest agent must reject any request that attempts to exceed immutable boot policy bounds, including the immutable `network_enabled` gate, either axis's ceiling, output caps, runtime caps, or workspace roots. A `set_mode` request above an axis `max` is rejected with `policy_denied`.

When a future HTTP control endpoint exposes this surface to remote callers, the per-axis ceiling is the defense-in-depth bound on what any connection can escalate to, and is required alongside — not instead of — endpoint authentication.

## Examples

### Locked-down Rust build VM

```toml
[policy]
network_enabled = false
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"

[policy.command]
default = "read_only"
max = "edit"
read_only = ["ls", "cat", "rg", "find"]
edit = ["cargo", "rustc", "git"]
```

This policy boots in `read_only` for safe inspection and lets the caller escalate to `edit` for builds and tests, but never to `yolo` — there is no reachable level that runs an arbitrary shell. No outbound network access.

The equivalent legacy form, `allowed_commands = ["cargo", "rustc", "git", "ls", "cat"]`, is still accepted and behaves as a fixed `edit` level with no escalation room.

### Network-enabled fetch VM

```toml
[policy]
network_enabled = true
allowed_commands = ["git", "curl", "ls", "cat"]
max_runtime_secs = 30
max_output_bytes = 262_144
workspace_path = "/workspace"
```

This policy permits network access, but only through the listed commands and within tighter runtime and output caps.

## Invalid Config Cases

### Missing policy table

```toml
network_enabled = false
allowed_commands = ["cargo"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
```

Invalid because all policy fields must be inside `[policy]`.

### Relative workspace path

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "workspace"
```

Invalid because `workspace_path` must be absolute.

### Non-positive caps

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo"]
max_runtime_secs = 0
max_output_bytes = -1
workspace_path = "/workspace"
```

Invalid because runtime and output caps must be positive integers.

### Shell snippet as command

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo test", "git status"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
```

Invalid because allowlist entries are executable names, not commands with arguments.

### Non-string or duplicate command entries

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo", "cargo", 42]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
```

Invalid because command entries must be unique strings.

### Runtime escalation fields

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"

[runtime_override]
network_enabled = true
allowed_commands = ["bash"]
```

Invalid because runtime override or escalation fields are not part of the schema.
