# petri-sandbox

Python client for the [Petri](https://github.com/squirrelsoft/petri) sandbox.

Thin wrapper over the `petri` CLI that exposes an E2B-style `Sandbox` object.
Standard library only; requires Python 3.10+.

## Installation

```bash
pip install petri-sandbox        # once published
# or from source:
pip install -e clients/python
```

Ensure the `petri` binary is on `PATH`, or set `PETRI_BIN` to its absolute path.

## Quick start

```python
from petri import Sandbox

# Create a sandbox from a template
sandbox = Sandbox.create(
    "base",
    workspace=".",
    policy="./policy.toml",
)

# Run a command — non-zero exits are results, not exceptions
result = sandbox.commands.run("cargo test", cwd="/workspace")
print(result.stdout)
print("exit code:", result.exit_code)

# Opt into exceptions for policy-denied / timeout / truncation / failure
result.raise_for_status()

# Connect to an existing sandbox by id
sandbox2 = Sandbox.connect(sandbox.sandbox_id)

# List all sandboxes
handles = Sandbox.list(state="running")
for h in handles:
    print(h.sandbox_id, h.state)

# Tear down
sandbox.kill()
# or statically:
Sandbox.kill(sandbox.sandbox_id)
```

## Binary resolution

The binary is resolved in this order:

1. `petri_path=` keyword argument on any SDK call
2. `PETRI_BIN` environment variable
3. `petri` on `PATH`

## Testing without a real binary

All SDK methods accept a `runner=` keyword argument — an injectable callable
`(argv, stdin) -> (stdout, stderr, returncode)` — so you can test without
spawning the real binary:

```python
from petri import Sandbox

def fake_runner(argv, stdin=None):
    return "sb-test\n", "", 0

sandbox = Sandbox.create(runner=fake_runner)
print(sandbox.sandbox_id)  # sb-test
```

## Error types

| Exception | When raised |
|---|---|
| `SandboxNotFoundError` | CLI stderr contains `no sandbox with id` |
| `SandboxNotReadyError` | CLI stderr contains `not running` |
| `PolicyDeniedError` | `raise_for_status()` + status `rejected` / `error.code == policy_denied` |
| `CommandTimeoutError` | `raise_for_status()` + status `timeout` |
| `CommandFailedError` | `raise_for_status()` + status `failure` (non-zero exit) |
| `OutputTruncatedError` | `raise_for_status()` + `output_truncated == True` |
| `ProtocolVersionMismatchError` | `frame.protocol_version != 1` |
| `PetriError` | any other CLI failure |
