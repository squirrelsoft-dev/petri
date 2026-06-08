# Protocol Schema

Petri protocol version `1` has a shared Rust source of truth in
`crates/petri-protocol`. The host crate and guest crate both depend on those
wire types instead of carrying independent request and result definitions.

The checked-in JSON Schema at `schema/petri-protocol-v1.schema.json` is the
language-client contract for TypeScript, Python, Go, and other SDKs. It covers
current dispatch requests, dispatch results, error frames, cancellation and
`set_mode` control frames, lifecycle/control operation names, filesystem
operation names, and the planned SDK module groups:

- `Sandbox`
- `Commands`
- `Filesystem`
- `Git`
- `Pty`
- `Template`

The schema intentionally reserves planned operation names before all runtime
handlers exist. A name appearing in the schema means clients can align naming
and type generation; it does not by itself mean the current guest implements
that operation.

## SDK Mapping

| SDK module | Current schema names | Planned schema names |
|---|---|---|
| `Sandbox` | `LifecycleControlRequest` for `sandbox.create`, `sandbox.connect`, `sandbox.list`, `sandbox.info`, `sandbox.running`, `sandbox.kill` | `sandbox.pause`, `sandbox.resume`, `sandbox.snapshot`, `sandbox.timeout` |
| `Commands` | `BashCommandRequest`, `DispatchResult`, `commands.run` | `commands.stream`, `commands.background`, `commands.stdin` |
| `Lsp` | `LspPositionRequest` (`lsp_hover`, `lsp_definition`, `lsp_references`), `LspDiagnosticsRequest` (`lsp_diagnostics`), `LspRenameRequest` (`lsp_rename`); results carry the structured `data` field | additional language tools |
| `Filesystem` | none | `FilesystemRequest` with `filesystem.exists`, `filesystem.get_info`, `filesystem.list`, `filesystem.read`, `filesystem.write`, `filesystem.write_files`, `filesystem.make_dir`, `filesystem.remove`, `filesystem.rename`, `filesystem.watch` |
| `Git` | none | `GitRequest` with `git.clone`, `git.status`, `git.diff`, `git.commit`, `git.checkout` |
| `Pty` | none | `PtyRequest` with `pty.open`, `pty.input`, `pty.resize`, `pty.close` |
| `Template` | none | `TemplateRequest` with `template.build`, `template.list`, `template.info`, `template.remove` |

## Framing

Petri uses newline-delimited JSON over vsock. Each request or control frame is
one UTF-8 JSON object followed by `\n`. Each response is one UTF-8 JSON object
followed by `\n`.

Every request has a non-empty `id`. Every result includes the matching `id`, or
`null` when malformed input did not contain a usable id. Guests may process
requests concurrently, so clients must correlate responses by `id` rather than
response order.

## Versioning

Every frame carries `protocol_version`. Version `1` is the only current version.
Guests reject unsupported versions with `status = "rejected"` and
`error.code = "unsupported_protocol_version"` when a usable request id is
available.

Compatible additions may add optional request fields, optional result fields,
new operation names, or new error metadata. Incompatible changes must increment
`protocol_version` and publish a new schema file.

## Fixtures

Shared fixtures live under `schema/fixtures`. Client implementations should
deserialize these files and assert that generated request/result types preserve
the same wire shape. The dispatch fixtures cover every request variant
(`bash_command`, `cancel`, `set_mode` for both axes, and the `lsp_*` family) and
the result variants (success, command failure, timeout, policy rejection,
malformed, and structured `data` results).

The fixtures are enforced as the contract, not just examples:

- `crates/petri-protocol/tests/schema_validation.rs` compiles
  `schema/petri-protocol-v1.schema.json` and validates **every** fixture against
  it, so a fixture that drifts from the published schema fails `cargo test`.
- The same test serializes frames built from the Rust constructors and validates
  those against the schema, so the hand-maintained schema cannot silently drift
  from the wire types. It also asserts the schema rejects invalid frames (wrong
  `protocol_version`, unknown status, missing required fields).
- The guest integration tests (`crates/petri-guest/tests/protocol_behavior.rs`)
  additionally feed the request fixtures through the live dispatch handler, so
  fixture drift is caught against runtime behavior as well.

Language clients should validate their generated payloads against the same
schema file to stay aligned across Rust, TypeScript, Python, and Go.
