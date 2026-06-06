# Immutable Policy Config

Each Petri VM boots with a TOML policy config that defines what the guest agent is allowed to do. The policy is loaded before the agent accepts dispatch requests and is immutable for the lifetime of the VM.

The policy is the maximum authority available inside the VM. A dispatch request may narrow limits for a single request, but it cannot enable network access, permit new commands, raise caps, or change workspace boundaries after boot.

## Schema

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo", "rustc", "git", "ls", "cat"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
```

| Field | Type | Required | Description |
|---|---:|---:|---|
| `network_enabled` | boolean | yes | Whether the VM may make outbound network connections. Defaults should be treated as deny if a future loader supports omitted fields. |
| `allowed_commands` | array of strings | yes | Executable names the guest agent may launch. Entries are command names, not shell snippets. |
| `max_runtime_secs` | positive integer | yes | Maximum wall-clock runtime for one dispatch. |
| `max_output_bytes` | positive integer | yes | Maximum combined stdout and stderr bytes returned for one dispatch. |
| `workspace_path` | absolute string path | yes | Canonical shared workspace root inside the guest. |

The `[policy]` table is required. Unknown fields are invalid unless a future schema version explicitly defines them.

## Network Controls

`network_enabled = false` means the VM has no outbound network access. This is the default safety posture and should be used for ordinary build, test, and inspection workloads.

`network_enabled = true` permits outbound network access at the VM policy layer. It does not bypass command allowlists, runtime caps, output caps, or workspace rules. If network access is enabled, the guest agent still rejects any dispatch that violates another policy field.

## Allowed Commands

`allowed_commands` is an executable allowlist. Each entry must be the executable name the guest agent is allowed to start, such as `"cargo"` or `"git"`.

Command entries are not shell text. For example, `"cargo"` is valid, but `"cargo test"` is invalid. Arguments are evaluated as part of the dispatch request and do not grant permission to execute additional binaries.

An empty allowlist is valid and means the guest agent must reject all process execution commands.

The guest agent should resolve the executable according to its configured process-launch rules, then enforce that the launched executable matches the allowlist entry. Shell metacharacters, path traversal, and command chaining must not be accepted as command names.

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

The guest agent loads the policy before accepting vsock dispatch requests. Once loaded, the policy remains fixed until the VM is torn down and a new VM boots with a new policy.

Host-provided dispatch messages cannot widen the policy. A dispatch may include narrower request-scoped limits, such as a shorter runtime cap, but the effective policy is always the most restrictive combination of the boot policy and request constraints.

The guest agent must reject any request that attempts to override immutable boot policy fields, including network access, command allowlists, output caps, runtime caps, or workspace roots.

## Examples

### Locked-down Rust build VM

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo", "rustc", "git", "ls", "cat"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
```

This policy supports local Rust builds and tests without outbound network access.

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
