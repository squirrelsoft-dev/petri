# petri

Host-side CLI and library skeleton for Petri.

The public library exposes lifecycle and dispatch boundaries through a backend
trait. The initial binary wires those commands to a stub backend, so command
shape can stabilize before macOS, Linux, or Windows VM implementations exist.

## CLI

```text
petri create --id <id> --workspace <path> --policy <path> [--image <path>] [--backend stub]
petri dispatch --id <id> --command <name> --cwd <path> [--request-id <id>] [--arg <value>]... [--timeout-ms <ms>] [--max-output-bytes <bytes>]
petri stop --id <id>
petri teardown --id <id>
```

`dispatch` currently models protocol version 1 `bash_command` requests. Backend
implementations are responsible for opening the guest transport, sending the
NDJSON frame, and returning the structured result.
