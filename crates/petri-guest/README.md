# petri-guest

Initial guest agent binary for Petri VMs.

This crate currently provides the skeleton needed to boot the agent, load the
immutable TOML policy, parse newline-delimited JSON dispatch requests, and write
structured result frames. Real process execution and platform vsock binding are
left for the next implementation step.

## Static Build Target Plan

The guest should be distributed as a static Linux binary in VM images:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build -p petri-guest --release --target x86_64-unknown-linux-musl
```

When ARM guest images are introduced, add the matching musl target:

```sh
rustup target add aarch64-unknown-linux-musl
cargo build -p petri-guest --release --target aarch64-unknown-linux-musl
```

The crate intentionally avoids async runtimes and dynamic system integrations at
this stage so the static target stays straightforward.

## Local Smoke Test

```sh
cargo run -p petri-guest -- --policy examples/policy.toml --transport stdio
```

Then send one request frame:

```json
{"protocol_version":1,"id":"req-1","tool":"bash_command","args":{"command":"cargo","argv":["test"],"cwd":"/workspace"}}
```

