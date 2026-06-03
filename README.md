# Petri

> **An isolated execution environment for autonomous agents.**

Petri is a lightweight microVM host that gives autonomous agents a safe, contained place to act. It manages the full lifecycle of OS-native microVMs — provisioning, sandboxing, workspace sync, and teardown — so that agents can execute code, run commands, and interact with the filesystem without touching the host.

Think of it as the dish the spore grows in: isolated, controlled, and purpose-built for what lives inside it.

---

## What It Does

- **Provisions and manages microVMs** on macOS (Apple Virtualization framework), Linux (Firecracker/KVM), and Windows (Hyper-V).
- **Hosts the Petri guest agent** — a minimal static binary that runs inside each VM, listens for tool dispatch requests over vsock, enforces sandbox policy, and returns structured results.
- **Shared workspace** — a virtio-fs mount gives the host and guest a shared `/workspace` directory. Files written inside the VM are instantly visible on the host. No explicit sync step.
- **Policy enforcement** — each VM boots with a policy config that governs network access, allowed commands, resource limits, and runtime caps. Policy is the VM's own constitution — not overridable by the host at runtime.
- **Clean lifecycle** — VMs are provisioned on demand, snapshotted, and torn down cleanly. The workspace survives the VM if you want it to.

---

## Where It Fits

Petri is part of the spore ecosystem:

| Project | Role |
|---|---|
| **[spore-core](../spore-core)** | The agentic harness runtime — the loop, tools, sandbox, memory, and the improvement flywheel. |
| **[spore](../spore)** | A micro-agent framework — single-responsibility agents built from a runtime plus a declarative skill file. |
| **[cordyceps](../cordyceps)** | A single autonomous, self-improving agent you deploy and control remotely. |
| **Petri** *(this project)* | The isolated execution environment agents run their tools inside. |
| **mycelium** *(future)* | Teams of agents working together. |

```
spore-core  ──►  the harness runtime
   spore    ──►  micro agents
 cordyceps  ──►  one autonomous operator
  mycelium  ──►  teams of agents
    petri   ──►  the dish everything grows in safely
```

spore-core's `PetriSandboxProvider` delegates all tool execution to a running Petri instance. The harness doesn't know or care what's inside the VM — it dispatches a tool call and gets a result back.

---

## How It Works

### Host ↔ Guest communication

Tool dispatch crosses the VM boundary over **vsock** (virtual socket — VM-native IPC, low latency, no network stack required). The protocol is newline-delimited JSON:

```json
// Dispatch request
{
  "id": "abc123",
  "protocol_version": 1,
  "tool": "bash_command",
  "args": { "command": "cargo", "argv": ["test"], "cwd": "/workspace" },
  "limits": { "timeout_ms": 30000 }
}

// Result
{
  "protocol_version": 1,
  "id": "abc123",
  "status": "success",
  "stdout": "running 42 tests...\ntest result: ok",
  "stderr": "",
  "exit_code": 0,
  "elapsed_ms": 4821,
  "output_truncated": false
}
```

See [Vsock Dispatch Protocol](docs/vsock-dispatch-protocol.md) for the canonical request and result schemas, framing rules, error shape, timeout and cancellation behavior, output limits, versioning, and compatibility rules.

### Workspace sync

The host and guest share a `/workspace` directory via **virtio-fs**. Writes inside the VM are instantly visible on the host — no transfer step, no polling. This is the same approach Apple's Containers app uses.

### Policy config

Each VM boots with an immutable policy that governs what it may do:

```toml
[policy]
network_enabled = false
allowed_commands = ["cargo", "rustc", "git", "ls", "cat"]
max_runtime_secs = 60
max_output_bytes = 1_048_576
workspace_path = "/workspace"
```

The guest agent enforces policy independently. A request that violates policy is rejected even if the host asks for it. See [Immutable Policy Config](docs/policy-config.md) for the canonical TOML schema, examples, and invalid config cases.

---

## Platform Support

| Platform | Backend |
|---|---|
| macOS | Apple Virtualization framework |
| Linux | Firecracker via KVM |
| Windows | Hyper-V *(planned)* |

---

## Components

**`petri`** — the host-side CLI and library. Provisions VMs, manages lifecycle, exposes the vsock dispatch interface.

**`petri-guest`** — the guest agent binary that runs inside each VM. Minimal, static, fast to start. Listens on vsock, executes tool calls, enforces policy, returns results.

**Base VM images** — pre-built images with `petri-guest` installed, ready to layer a workspace on top. Build tooling provided for teams that want custom or auditable images.

---

## Status

🌱 **Early planning.** Architecture is being designed against spore-core's sandbox seam. The vsock protocol and guest agent spec will be stabilized before platform implementations begin.

---

## Safety

Petri's isolation guarantee depends on the VM boundary, not convention:

- **Network off by default** — the VM has no outbound network access unless the policy explicitly enables it.
- **Policy is immutable at runtime** — the host cannot escalate permissions after the VM boots.
- **Workspace is the only shared surface** — nothing else crosses the VM boundary except vsock dispatch results.
- **Guest agent is minimal and auditable** — a small static Rust binary with no runtime dependencies. Read it in an afternoon.

---

## License

[MIT](LICENSE) © SquirrelSoft LLC
