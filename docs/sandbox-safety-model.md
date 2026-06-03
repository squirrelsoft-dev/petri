# Sandbox Safety Model

Petri provides an isolated execution environment for autonomous agents by running tool work inside a policy-bound microVM. The safety model is built around a small number of explicit boundaries:

- a VM boundary between host and guest
- an immutable boot policy enforced by the guest agent
- one shared workspace mount
- a narrow vsock request/result interface
- network access disabled unless policy enables it

This document defines the security expectations Petri must preserve as implementation work begins.

## Security Goals

Petri is expected to:

- keep guest processes isolated from host processes, host credentials, host devices, and host files outside the configured workspace
- make the workspace the only shared filesystem surface between host and guest
- prevent a dispatch request from widening the VM's boot policy
- prevent unbounded command runtime and unbounded result output
- make outbound network access opt-in, visible in policy, and disabled by default
- return structured execution results without exposing additional host control surfaces

Petri's safety guarantees depend on the VM boundary and the guest agent policy checks. They do not depend on an agent voluntarily behaving well.

## Threat Model

Petri assumes the workload inside the guest may be adversarial or compromised. This includes:

- generated code that attempts to read secrets, modify unexpected files, or persist outside the workspace
- build scripts and test fixtures that attempt command injection, path traversal, or symlink escape
- tools that attempt to run longer than allowed or emit excessive output
- workloads that attempt outbound network access when network policy denies it
- guest processes that attempt to exploit the guest agent, guest OS, shared filesystem, vsock protocol, or virtualization backend

Petri also assumes the host-side caller may send malformed, unsupported, or policy-violating dispatch requests. The guest agent must reject these requests instead of trusting the host to pre-filter them.

## Trust Boundaries

### Host

The host provisions VMs, mounts the workspace, sends dispatch requests, and receives results. The host is trusted to select the VM image, policy config, workspace directory, and lifecycle controls.

The host is not trusted to widen policy after boot. Once the guest agent loads policy, host requests can only ask for work inside that policy. A request may narrow runtime or output limits for a single dispatch, but it cannot enable network access, add commands, raise caps, or change workspace roots.

### Guest VM

The guest VM contains the workload, the guest operating system, and the `petri-guest` agent. Guest workload code is untrusted. The guest agent is part of the trusted computing base for policy enforcement inside the VM.

The VM boundary is the primary containment boundary. A guest process escape from the VM is considered a critical break of the safety model.

### Shared Workspace

The workspace is intentionally shared. Files written by the guest are visible to the host, and files present on the host side of the workspace are visible to the guest.

The workspace is not a secret boundary. Anything placed in the workspace should be treated as readable and writable by guest workload code allowed to operate there.

### Vsock Dispatch Interface

Vsock is the only command and result transport between host and guest. It is not a general host API. The guest agent accepts structured dispatch frames, applies policy, executes allowed work, and returns structured result frames.

The vsock result surface must remain narrow: stdout, stderr, exit status, elapsed time, truncation state, and machine-readable errors. Result frames must not include host-local paths, host credentials, opaque host handles, or additional capabilities.

## VM Isolation Guarantee

Petri's isolation guarantee is: guest workload execution is contained within a microVM, and the guest can interact with the host only through the configured workspace and the vsock result protocol.

This guarantee requires:

- no host filesystem mounts other than the configured workspace unless a future policy explicitly defines them
- no host process namespace sharing
- no host IPC, device, credential, or environment-variable passthrough by default
- network devices absent or blocked when policy disables network access
- VM teardown that removes guest runtime state unless an explicit snapshot or workspace retention path preserves it

Platform backends may differ in implementation details, but they must preserve the same external safety contract.

## Workspace Risks

The shared workspace is useful and intentionally risky. It gives the guest a way to produce useful artifacts, but it also gives untrusted code a way to modify files the host can see.

Expected risks include:

- source files, generated files, or build outputs may be modified by the guest
- malicious files may be written into the workspace
- symlinks inside the workspace may point outside the workspace
- filenames and file contents may attempt to confuse host tools or reviewers
- secrets accidentally placed in the workspace may be read by the guest

Petri mitigates these risks by requiring guest working directories to canonicalize inside the workspace root and by treating the workspace as the only shared filesystem surface. Petri does not make arbitrary workspace contents safe to execute or open on the host.

Host-side workflows should treat workspace diffs as untrusted output. Review changes before committing them, avoid placing secrets in the workspace, and avoid running generated artifacts on the host without separate validation.

## Vsock Result Surface

The dispatch protocol is a request/result protocol, not an interactive shell bridge. Results are bounded and structured according to [Vsock Dispatch Protocol](vsock-dispatch-protocol.md).

The guest agent must:

- correlate each result to a request id
- reject malformed frames and unsupported protocol versions
- reject unknown tools and policy violations
- enforce runtime and output caps before returning results
- truncate output according to the effective policy
- avoid returning unbounded process output
- avoid adding fields that grant host-side authority

The host must treat stdout and stderr as untrusted text. Output may contain terminal escapes, misleading diagnostics, or generated instructions, and should be rendered or logged defensively by host applications.

## Network Policy

Network access is off by default. A normal Petri VM should boot without outbound network capability unless the immutable policy explicitly sets `network_enabled = true`.

When network access is disabled:

- guest workload commands must not be able to make outbound network connections
- dispatch requests cannot enable network access
- command allowlists do not imply network permission

When network access is enabled:

- command allowlists, workspace checks, runtime caps, and output caps still apply
- the policy should be considered higher risk and reviewed accordingly
- callers should assume workloads can exfiltrate any data readable inside the guest, including workspace contents

Network policy is documented in [Immutable Policy Config](policy-config.md).

## Non-Goals

Petri does not claim to:

- protect secrets intentionally placed in the shared workspace
- make guest-produced files safe for host execution
- prevent malicious changes to files the guest is allowed to write
- provide confidentiality from the host operator or host process that provisions the VM
- protect against a compromised host OS, malicious hypervisor, or malicious VM image selected by the host
- provide perfect protection against unknown virtualization, kernel, filesystem, or CPU vulnerabilities
- enforce semantic safety of generated code, test output, or model instructions
- replace code review, dependency review, artifact scanning, or host endpoint security

## Host Assumptions

Petri assumes:

- the host OS and virtualization backend are trusted and patched
- the host-side Petri process is trusted to provision the intended VM image and policy
- the VM image is built from trusted inputs or reviewed before use
- host credentials are not mounted into the guest unless an explicit future policy allows it
- the selected workspace does not contain secrets or files the guest must not read
- host applications render guest output defensively
- users review workspace changes before committing, publishing, or executing them on the host

If any of these assumptions are false, Petri may still reduce blast radius, but its documented safety guarantee no longer holds in full.
