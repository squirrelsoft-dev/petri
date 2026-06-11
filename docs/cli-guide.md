# Petri CLI Guide

A task-oriented tour of the `petri` command line: which command to reach for,
how the families fit together, and copy-pasteable recipes for common jobs.

This is the *how-to*. For the underlying contracts see:
[Immutable Policy Config](policy-config.md) (the `--policy` schema and
templates), [Sandbox SDK API](sdk-api.md) (the programmatic surface),
[Vsock Dispatch Protocol](vsock-dispatch-protocol.md) (the wire format), and
[Base VM Images](base-vm-images.md) (image building).

> **Prerequisite.** Running real sandboxes needs the macOS backend and a built
> `petri` binary (`cargo build -p petri`, then `target/debug/petri`). The
> examples below use `petri`; substitute your binary path, or put it on `PATH`.

## The mental model

Everything lives under three command families plus a few compatibility aliases:

| Family | Manages | You use it to… |
|---|---|---|
| `petri sandbox` | **running VMs** | create, list, exec into, connect to, and kill sandboxes |
| `petri image`   | **disk images** | build base images and manage the named-image registry (`~/.petri/images`) |
| `petri policy`  | **policy templates** | curate reusable boot policies (`~/.petri/policies`) |

The sandbox is the unit of work; an **image** is what it boots from; a **policy**
is what it's allowed to do. A typical flow is: pick (or write) a policy → boot a
sandbox from an image with that policy → `exec` commands → `kill`.

### Compatibility aliases

The older low-level verbs still work and map onto the families above. Prefer the
`sandbox` forms; reach for the aliases only when you need their specific
behavior.

| Alias | Equivalent / purpose |
|---|---|
| `petri create --id <id> …` | `petri sandbox create` but **requires** `--id` and prints a human message instead of just the id |
| `petri dispatch …` | low-level protocol access — the only way to call **non-bash tools** (`lsp_*`) |
| `petri stop --id <id>` | graceful guest stop of one instance (no NBD/registry cleanup) |
| `petri teardown --id <id>` | force-remove one instance (no `--all` / `--purge`) |
| `petri image build` | the legacy image-bundle pipeline (distinct from `petri image create`) |

## Which command do I use?

The overlaps that trip people up, disambiguated:

**`petri sandbox create` vs `petri create`** — same underlying operation
(`backend.create`). `sandbox create` generates an id when you omit `--id` and
prints **just the sandbox id** (script-friendly); `create` requires `--id` and
prints `created instance …`. Use `sandbox create`.

**`petri sandbox exec` vs `petri dispatch`** — `exec` is the ergonomic command
runner: it maps to a `bash_command` dispatch and takes the command as trailing
positionals. `dispatch` is the raw protocol surface and the **only** way to
invoke the LSP tools (`lsp_hover`, `lsp_definition`, `lsp_references`,
`lsp_diagnostics`, `lsp_rename`). For running shell commands, use `exec`.

**`petri sandbox kill` vs `petri stop` / `petri teardown`** — `kill` is the
recommended teardown: it removes runtime state **and** cleans up the per-sandbox
NBD daemon, supports `--all`, and `--purge` (also delete the scratch disk).
`stop` asks the guest to shut down gracefully; `teardown` force-removes a single
instance. Both aliases skip the NBD/registry cleanup that `kill` does.

**`petri image create` vs `petri image build`** — `create` manages the
**named-image registry** (a base layer + per-image scratch overlay under
`~/.petri/images`). `build` is the legacy end-to-end bundle pipeline. New work
uses `create` / `freeze` / `rebuild`.

**Three ways `sandbox create` boots** — pick by the flags:

| Flags | Boots from | Use when |
|---|---|---|
| (default) or `[base]` positional | the backend's default base image (or `--image <path>`) | you just want a sandbox |
| `--base <name>:<tag>` | a frozen layer in the named-image registry, over a fresh per-sandbox scratch | you froze a custom base and want to run against it |
| `--bootstrap <name>:scratch --disk <nocloud>` | a disposable EFI builder VM | you're *building/provisioning* an image, not running a workload |

## Recipes

### Run a command in a fresh sandbox

```sh
# Boot a sandbox (prints its id), then exec against it.
id=$(petri sandbox create --workspace . --policy developer)
petri sandbox exec "$id" -- cargo test
petri sandbox kill "$id"
```

`--policy` accepts a **template name** (`developer`, `locked-down`, `yolo`,
`fetch`, or one of your own) or a path to a `.toml` file. See
[Choosing a policy](#choosing-a-policy).

### Pass arguments and stdin

`exec` takes the command and its args as trailing positionals. Use `--` so a
command starting with `-` isn't parsed as a flag. Stdin is piped through:

```sh
petri sandbox exec "$id" --cwd /workspace -- rg --json "TODO"
echo "hello" | petri sandbox exec "$id" -- cat
```

Per-request caps (never exceed the boot policy):

```sh
petri sandbox exec "$id" --timeout-ms 5000 --max-output-bytes 65536 \
  --env RUST_LOG=debug,CI=1 -- cargo build
```

### List and filter sandboxes

```sh
petri sandbox list                                  # pretty table
petri sandbox list --state running --format json    # machine-readable
petri sandbox list --metadata project=acme --limit 20
```

`--metadata` filters by key=value pairs you set at create time
(`petri sandbox create … --metadata project=acme,owner=me`).

### Connect to an existing sandbox

```sh
petri sandbox connect "$id"      # readiness check; errors if missing / not running
```

`connect` never tears anything down — it just verifies the sandbox is reachable.

### Tear down

```sh
petri sandbox kill "$id"          # one sandbox (+ NBD cleanup)
petri sandbox kill --all          # every sandbox
petri sandbox kill --purge "$id"  # also delete its per-sandbox scratch disk
```

### Choosing a policy

List what's available, inspect one, and use it by name:

```sh
petri policy list                 # built-ins + your templates, with posture
petri policy show developer       # print the TOML
petri sandbox create --workspace . --policy developer
```

Built-ins: `locked-down` (no network, read-only), `developer` (no network,
read-only → edit build tools), `yolo` (full egress, unrestricted), `fetch`
(network on, curated fetch tools). Full schema and semantics:
[Immutable Policy Config](policy-config.md).

### Create and customize a policy

```sh
# New user template, seeded from an existing one (defaults to --from locked-down).
petri policy create my-ci --from developer
petri policy edit my-ci                 # opens $EDITOR; re-validates on save
petri sandbox create --workspace . --policy my-ci

# Editing a built-in forks a private copy first (copy-on-write):
petri policy edit developer             # writes ~/.petri/policies/developer.toml
petri policy remove developer           # removes the override; built-in returns

# Print a template's path for tools that need a file:
petri sandbox create --workspace . --policy "$(petri policy path developer)"
```

A template that matches a built-in name **shadows** it; `remove` on a built-in
without an override is rejected. Templates are validated against the policy
schema on create/edit, so the registry never holds a policy that fails at boot.

### Build a base image and boot from it

The named-image registry workflow (see [Base VM Images](base-vm-images.md) for
the full pipeline):

```sh
# Provision and seal a frozen base layer from a nocloud EFI image.
petri image create trixie --from-nocloud ./debian-nocloud.raw \
  --tag base --provision ./provision.sh

petri image list                        # registry contents (scratch + frozen layers)
petri image inspect trixie:base         # full metadata for one layer

# Boot a sandbox from the frozen base over a fresh per-sandbox scratch.
petri sandbox create my-sbx --base trixie:base --workspace . --policy developer
```

Iterate on an image with `petri image freeze <name>:scratch --tag <tag>` to seal
the live scratch, or `petri image rebuild` to re-provision a layer. Remove with
`petri image delete <name>:<tag>`.

## Getting help

`petri --help` prints the full surface; every family and subcommand accepts
`--help` (`petri sandbox create --help`, `petri policy --help`, …).
