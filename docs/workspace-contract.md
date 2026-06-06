# Workspace Mounting Contract

Petri exposes exactly one shared filesystem surface between host and guest: the configured workspace.

## Host Path

The host-side `--workspace` path must be:

- non-empty
- absolute
- present before VM creation
- a directory

The host library canonicalizes the workspace path before passing it to a backend. Missing paths, relative paths, and non-directory paths are rejected during `InstanceConfig` validation before a VM is started.

## Guest Mapping

Every backend must present the validated host workspace at this guest path:

```text
/workspace
```

On the macOS backend, this is a writable virtio-fs share using tag `workspace`. The policy `workspace_path` must resolve to the same guest mount path, normally `/workspace`.

## File Visibility

The workspace is shared live. Files and directories that exist in the host workspace before VM boot are visible to guest workload code after the workspace mount is ready. Files written, modified, or removed by guest workload code under `/workspace` are visible on the host through the original workspace directory.

Petri does not provide a copy or sync layer for this surface. Host and guest observe the same shared directory, subject to normal filesystem and virtio-fs behavior.

## Persistence

Workspace contents persist after VM stop or teardown because the workspace is host-owned. Teardown removes Petri runtime state for the VM, not files in the configured workspace.

Callers that want an ephemeral workspace must create a temporary host directory before `create` and remove it themselves after teardown.

## Safety Expectations

The workspace is not a confidentiality boundary. Anything placed in the workspace should be treated as readable and writable by allowed guest workload code.

Host workflows should treat workspace changes as untrusted output. Review changes before committing them, avoid placing secrets in the workspace, and avoid executing guest-produced files on the host without separate validation.
