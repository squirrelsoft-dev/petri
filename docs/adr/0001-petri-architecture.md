# ADR 0001: Petri Architecture

## Status

Accepted

## Context

Petri provides an isolated execution environment for autonomous agents. It must let a host runtime dispatch tool work into a contained VM, observe structured results, and preserve a narrow trust boundary between host state and untrusted guest workload code.

The initial architecture needs to work across macOS, Linux, and Windows backends while presenting the same safety and lifecycle contract to callers. Platform implementations may differ internally, but spore-core and other callers should not need platform-specific knowledge to use Petri.

This ADR defines the initial host/guest split, platform backend contract, VM lifecycle model, trust assumptions, threat boundaries, and integration point with spore-core.

## Decision

Petri is split into a trusted host controller, a trusted guest agent, platform-specific VM backends, and an intentionally shared workspace.

```text
spore-core
   |
   | PetriSandboxProvider
   v
petri host API / CLI
   |
   | platform backend
   v
microVM
   |
   | vsock dispatch
   v
petri-guest
   |
   | allowed process launches
   v
untrusted workload in /workspace
```

The host owns VM provisioning and lifecycle. The guest owns runtime policy enforcement for dispatch requests. Platform backends own OS-specific VM creation, vsock wiring, workspace mounting, resource configuration, and teardown. The only normal cross-boundary surfaces are the shared workspace and the vsock request/result protocol.

## Host Responsibilities

The host-side `petri` library and CLI are responsible for:

- selecting the VM image and platform backend
- preparing the shared workspace mount
- providing the immutable boot policy to the VM
- starting, stopping, snapshotting, and tearing down VMs
- opening the vsock dispatch connection to `petri-guest`
- translating caller tool execution requests into protocol frames
- enforcing host-side lifecycle timeouts and surfacing structured results

The host is trusted to choose the intended image, policy, workspace, and backend. It is not trusted to widen guest authority after boot. Runtime dispatch requests may narrow limits, but they cannot enable network access, add commands, raise caps, or change workspace roots.

## Guest Responsibilities

The `petri-guest` binary runs inside the VM and is part of Petri's trusted computing base. It is responsible for:

- loading and validating the immutable boot policy before accepting dispatch
- listening for dispatch requests over vsock
- validating protocol version, request shape, tool name, working directory, and request limits
- enforcing command allowlists, runtime caps, output caps, workspace boundaries, and network policy
- launching allowed workload processes without shell interpretation
- terminating timed-out or cancelled process trees
- returning bounded structured results

The guest must reject malformed, unsupported, unknown, or policy-violating requests even when the host sends them. Policy enforcement lives in the guest because untrusted workload code executes in the same VM and because the host dispatch channel must not be an escalation mechanism.

## Platform Backend Responsibilities

Each platform backend implements the same logical VM contract:

| Platform | Backend | Responsibility |
|---|---|---|
| macOS | Apple Virtualization framework | Create and manage microVMs, configure virtio-fs, provide vsock connectivity, and apply macOS-specific resource controls. |
| Linux | Firecracker/KVM | Create and manage Firecracker VMs, configure KVM resources, attach workspace storage, expose vsock, and clean runtime state. |
| Windows | Hyper-V | Planned backend for the same lifecycle, workspace, policy, and vsock contract. |

Backends may choose different image formats, boot mechanisms, snapshot formats, and resource-control primitives. They must preserve the external Petri contract:

- one configured workspace is the only shared filesystem surface
- vsock is the dispatch and result transport
- network access is disabled unless policy enables it
- guest runtime state is removed on teardown unless an explicit snapshot preserves it
- callers see consistent lifecycle states and result semantics across platforms

## VM Lifecycle Model

A Petri VM moves through these lifecycle states:

```text
configured -> starting -> ready -> running-dispatch -> idle -> stopping -> stopped
                                      |                         |
                                      v                         v
                                    failed <---------------- teardown
```

Lifecycle phases:

1. Configure: the host selects image, workspace, policy, resources, and backend.
2. Start: the backend boots the VM, mounts the workspace, injects or exposes the policy, and starts `petri-guest`.
3. Ready: the guest has loaded policy and is accepting vsock dispatch.
4. Dispatch: the host sends request frames; the guest validates policy and runs allowed work.
5. Idle: the VM remains available for more dispatches under the same immutable policy.
6. Stop: the host asks the backend to shut down the VM cleanly.
7. Teardown: backend runtime state is removed; the workspace may remain according to host configuration.

Snapshots are allowed only when they preserve the same safety contract. A restored snapshot must not silently widen policy or expose stale host credentials, mounts, network devices, or hidden state outside the documented lifecycle model.

## Trust Assumptions And Threat Boundaries

Petri trusts:

- the host OS, virtualization backend, and host-side Petri process
- the selected VM image and the `petri-guest` binary
- the immutable boot policy selected before VM startup

Petri does not trust:

- generated code, build scripts, tests, or commands running in the guest
- files written by guest workload code into the shared workspace
- stdout, stderr, diagnostics, or other text returned by workload processes
- host dispatch requests to be well-formed or policy-compliant

The VM boundary is the primary isolation boundary. The shared workspace is not a confidentiality boundary; anything in it should be treated as visible to guest workload code. The vsock interface is a structured dispatch API, not a shell bridge or general host API.

Petri's threat model and non-goals are defined in [Sandbox Safety Model](../sandbox-safety-model.md). The immutable runtime authority model is defined in [Immutable Policy Config](../policy-config.md), and dispatch framing is defined in [Vsock Dispatch Protocol](../vsock-dispatch-protocol.md).

## spore-core Integration

spore-core integrates with Petri through a `PetriSandboxProvider`. The provider owns a Petri VM handle and delegates sandboxed tool execution to Petri instead of executing tools directly on the host.

The integration boundary is intentionally small:

- spore-core requests tool execution with command, arguments, working directory, and optional narrower limits
- Petri translates the request to the vsock dispatch protocol
- `petri-guest` enforces policy and executes allowed work inside `/workspace`
- Petri returns structured status, stdout, stderr, exit code, elapsed time, truncation state, and errors
- spore-core treats returned output and workspace changes as untrusted results

spore-core should not depend on a specific VM backend, guest OS, image format, snapshot format, or process-launch implementation. Petri owns those details behind its sandbox provider boundary.

## Consequences

This architecture keeps Petri's public contract small and portable. It lets platform backends evolve independently while requiring them to preserve the same lifecycle, workspace, policy, and dispatch semantics.

The tradeoff is that `petri-guest` becomes security-critical. Bugs in policy loading, path canonicalization, process launching, output bounding, or cancellation can weaken Petri's safety guarantees even when the VM boundary remains intact.

Early implementation should prioritize:

- a minimal host API that models lifecycle states explicitly
- a small backend trait with conformance tests for workspace, vsock, network, and teardown behavior
- a minimal `petri-guest` binary with strict policy validation
- protocol compatibility tests shared by host and guest
- documentation that treats workspace contents and result output as untrusted
