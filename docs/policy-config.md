# Immutable Policy Config

Each Petri VM boots with a TOML policy config that defines what the guest agent is allowed to do. The policy is loaded before the agent accepts dispatch requests and is immutable for the lifetime of the VM.

The policy is the maximum authority available inside the VM. A dispatch request may narrow limits for a single request, but it cannot enable network access, permit new commands, raise caps, or change workspace boundaries after boot.

Command authority is expressed as an ordered **capability axis** with a boot-default level and an escalation **ceiling**. A live VM may move its active level up to the ceiling via the `set_mode` control frame — without a reboot — but never past it. The ceiling, not the starting level, is the immutable maximum. This is the mechanism described in [ADR 0002](adr/0002-policy-modes-and-runtime-mode-switching.md); see [Command Modes](#command-modes) below.

## Schema

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
edit = ["cargo", "rustc", "git", "sed", "tee"]
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `network_enabled` | boolean | yes | Whether the VM may make outbound network connections. Defaults should be treated as deny if a future loader supports omitted fields. |
| `[policy.command]` | table | yes\* | Command capability axis. See [Command Modes](#command-modes). |
| `allowed_commands` | array of strings | yes\* | Legacy flat allowlist. Mutually exclusive with `[policy.command]`. Entries are command names, not shell snippets. |
| `max_runtime_secs` | positive integer | yes | Maximum wall-clock runtime for one dispatch. |
| `max_output_bytes` | positive integer | yes | Maximum combined stdout and stderr bytes returned for one dispatch. |
| `workspace_path` | absolute string path | yes | Canonical shared workspace root inside the guest. |

\* Exactly one of `[policy.command]` or `allowed_commands` must be present. Setting both, or neither, is invalid.

The `[policy]` table is required. Unknown fields are invalid unless a future schema version explicitly defines them.

## Network Controls

`network_enabled = false` means the VM has no outbound network access. This is the default safety posture and should be used for ordinary build, test, and inspection workloads.

`network_enabled = true` permits outbound network access at the VM policy layer. It does not bypass command allowlists, runtime caps, output caps, or workspace rules. If network access is enabled, the guest agent still rejects any dispatch that violates another policy field.

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

A running VM moves its active command level with the `set_mode` control frame, with no reboot. The trusted host caller may climb toward `max` or drop back down; the guest rejects any request above `max` with `policy_denied`. The frame and its semantics are defined in [Vsock Dispatch Protocol](vsock-dispatch-protocol.md#mode-switching).

This is a control-plane action by the trusted host. The untrusted guest workload cannot emit dispatch or control frames, so it can never issue `set_mode` or otherwise escalate its own authority. See [Runtime Immutability](#runtime-immutability).

### Legacy Allowlist

A policy may instead set a flat `allowed_commands = [...]` array. This maps to a fixed `edit`-level axis with no escalation room (`default == max == edit`), preserving the original single-allowlist behavior. It is mutually exclusive with `[policy.command]`. An empty `allowed_commands` (or a `none` default with empty sets) means the guest rejects all process execution.

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
- **Host-driven mode transitions.** The trusted host caller may move the active command level between boot-declared levels with `set_mode`, up to but never past the axis `max`. This is a control-plane action by the trusted principal within authority the boot policy already granted — it does not widen the ceiling. The boot policy's per-axis `max` is the immutable maximum; `default` is merely where the VM starts.

The guest agent must reject any request that attempts to exceed immutable boot policy bounds, including network access, the command axis ceiling, output caps, runtime caps, or workspace roots. A `set_mode` request above `max` is rejected with `policy_denied`.

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
