# petri-go

Go client for the [Petri](https://github.com/squirrelsoft/petri) sandbox system.

Implements the language-agnostic SDK contract from `docs/sdk-api.md` as a thin
wrapper over the `petri` CLI. No third-party dependencies.

## Install

```
go get github.com/squirrelsoft/petri-go
```

## Usage

```go
package main

import (
    "context"
    "fmt"
    "log"

    petri "github.com/squirrelsoft/petri-go"
)

func main() {
    ctx := context.Background()

    // Create a sandbox from the "base" template.
    sb, err := petri.Create(ctx, "base", petri.CreateOptions{
        Workspace: ".",
        Policy:    "./policy.toml",
    })
    if err != nil {
        log.Fatal(err)
    }
    defer sb.Kill(ctx)

    // Run a command.
    result, err := sb.Commands().Run(ctx, "cargo test", petri.RunOptions{
        Cwd:       "/workspace",
        TimeoutMs: 60_000,
    })
    if err != nil {
        log.Fatal(err) // transport/protocol error
    }

    fmt.Println(result.Stdout)

    // Opt in to typed errors for policy/timeout/truncation.
    if err := result.Check(); err != nil {
        log.Fatal(err)
    }
}
```

## Binary resolution

The client resolves the `petri` binary in this order:

1. `CreateOptions.PetriPath` (or the equivalent option on other calls)
2. `PETRI_BIN` environment variable
3. `petri` on `PATH`

## Errors

All errors implement `errors.Is` against the package-level sentinels:

| Sentinel | When |
|---|---|
| `ErrSandboxNotFound` | CLI stderr contains `no sandbox with id` |
| `ErrSandboxNotReady` | CLI stderr contains `not running` |
| `ErrPolicyDenied` | `result.Check()` and status `rejected` |
| `ErrCommandTimeout` | `result.Check()` and status `timeout` |
| `ErrCommandFailed` | `result.Check()` and status `failure` |
| `ErrOutputTruncated` | `result.Check()` and output truncated |
| `ErrProtocolVersionMismatch` | frame `protocol_version != 1` |
| `ErrNotImplemented` | `files`, `git`, `pty` (reserved in v1) |

## Testing

The `Runner` function type is injectable so tests never spawn the real binary:

```go
sb := &petri.Sandbox{SandboxID: "test-sb", Runner: myFakeRunner}
```

Run the test suite:

```
cd clients/go && go test ./...
```
