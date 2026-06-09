# Petri first-party clients (#26)

Blessed, thin client packages that expose the E2B-style `Sandbox` SDK shape in
each language used by spore-core. They implement the language-agnostic contract
in [`docs/sdk-api.md`](../docs/sdk-api.md); the Rust reference implementation
lives in [`crates/petri/src/sdk.rs`](../crates/petri/src/sdk.rs).

| Language   | Location                              | Status |
|------------|---------------------------------------|--------|
| Rust       | [`crates/petri`](../crates/petri)     | reference (in-process backend) |
| TypeScript | [`clients/typescript`](./typescript)  | lifecycle + `commands.run` |
| Python     | [`clients/python`](./python)          | lifecycle + `commands.run` |
| Go         | [`clients/go`](./go)                  | lifecycle + `commands.run` |

## Transport

Petri has no remote HTTP control plane yet (it is on the roadmap). Today the
host backend is in-process and macOS-only, driven through the `petri` CLI. The
non-Rust clients are therefore **thin wrappers over the `petri` binary**: each
SDK call shells out to `petri sandbox ...`, captures stdout/stderr/exit code,
and parses the CLI's JSON output. When the HTTP control plane lands, only the
transport layer in each client changes — the public `Sandbox` surface stays the
same.

Every client resolves the binary in this order: an explicit option
(`petriPath` / `petri_path` / `PetriPath`), then the `PETRI_BIN` environment
variable, then `petri` on `PATH`. Every client also accepts an injectable
"runner" so tests can exercise the full surface without a real binary or VM.

## CLI mapping

| SDK call | CLI invocation |
|---|---|
| `Sandbox.create(template, opts)` | `petri sandbox create [template] --workspace <ws> --policy <policy> [--id <id>] [--backend <b>] [--image <p>] [--metadata k=v,...]` → stdout is the sandbox id |
| `Sandbox.connect(id)` | `petri sandbox connect <id>` (readiness check; errors if missing / not running) |
| `Sandbox.list(opts?)` | `petri sandbox list --format json [--state running] [--metadata k=v,...] [--limit N]` → stdout is a JSON array of handles |
| `Sandbox.kill(id)` / `sandbox.kill()` | `petri sandbox kill <id>` |
| `sandbox.commands.run(cmd, opts?)` | `petri sandbox exec <id> [--cwd <d>] [--env k=v,...] [--timeout-ms N] [--max-output-bytes N] [--request-id <r>] -- <cmd> [args...]`; `stdin` is piped to the child; stdout is a JSON `ResultFrame` |

Notes:
- `exec` takes the command and its args as **trailing positionals** after the
  sandbox id (there is no separate `--args` flag). Stop option parsing with `--`
  before the command so a command starting with `-` is not read as a flag.
- `exec` has no `--stdin` flag; `stdin` is delivered over the child's stdin.
- `--background` and `--user` are rejected by the CLI as not-yet-implemented;
  clients should not surface them in v1.
- The `petri` CLI prints errors as `petri: <message>` on stderr and exits 1.
  Usage text is printed on stdout with exit 0.

## `ResultFrame` (exec stdout)

```jsonc
{
  "protocol_version": 1,
  "id": "sandbox-exec-1",
  "status": "success",          // success | failure | rejected | timeout | cancelled | malformed
  "elapsed_ms": 1,
  "stdout": "hello",            // omitted for non-process tools
  "stderr": "",
  "exit_code": 0,               // integer, null, or omitted
  "output_truncated": false,
  "error": {                    // present for rejected/timeout/malformed
    "code": "policy_denied",
    "message": "command is not allowed by policy",
    "details": { "field": "args.command", "command": "curl" }
  }
}
```

### `CommandResult` (SDK view)

`commands.run` flattens the frame into a `CommandResult`:

| Field | Type | Derivation |
|---|---|---|
| `status` | enum | `frame.status` |
| `exitCode` | int \| null | `frame.exit_code` (null when absent) |
| `stdout` | string | `frame.stdout ?? ""` |
| `stderr` | string | `frame.stderr ?? ""` |
| `outputTruncated` | bool | `frame.output_truncated ?? false` |
| `error` | ErrorFrame \| null | `frame.error` |
| `success` | bool (derived) | `status == success && exitCode == 0` |

`run` does **not** throw for a non-success status or a non-zero exit — those are
normal results (matching the Rust reference). It throws only on transport/usage
failures (binary missing, malformed JSON, protocol version mismatch). Callers
who want exceptions on policy-denied / timeout / truncation can opt in via the
result's `check()` / `raise_for_status()` helper.

## Typed errors

A single base error (`PetriError`) with these subclasses, raised consistently
across languages:

| Error | When |
|---|---|
| `SandboxNotFoundError` | CLI stderr contains `no sandbox with id` |
| `SandboxNotReadyError` | CLI stderr contains `not running` |
| `PolicyDeniedError` | `result.check()` and `status == rejected` / `error.code == policy_denied` |
| `CommandTimeoutError` | `result.check()` and `status == timeout` |
| `OutputTruncatedError` | `result.check()` and `output_truncated == true` |
| `CommandFailedError` | `result.check()` and `status == failure` (non-zero exit) |
| `AuthorizationError` | `error.code` is an auth/capability code (reserved; e.g. `capability_denied`) |
| `ProtocolVersionMismatchError` | `frame.protocol_version != 1` |
| `PetriError` (base) | any other CLI failure (non-zero exit with an unrecognized message) |

`check()` raises the first applicable error in this order: protocol mismatch →
rejected/policy → timeout → command-failed → truncated; it is a no-op on a
clean success. `outputTruncated` can be true alongside a success status, so it
is checked last.

## Reserved modules

`files`, `git`, and `pty` are named on the `Sandbox` surface so the API is
stable and future work slots in without breaking callers. Their operations
raise a "not implemented in v1" error today.
