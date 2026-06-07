# Spike #36 — host-enforced, live-mutable VM egress filter

Status: **complete — superseded.** Tracking issue: #36. Design: [ADR 0002](../adr/0002-policy-modes-and-runtime-mode-switching.md).

> **Outcome:** the host-side smoltcp datapath worked (allow/deny + live switch
> ✅) but topped out at ~350 Mbit/s vs. ~46 Gbit/s for Apple's in-framework NAT
> (see Results below). We are **pivoting to in-guest enforcement**: keep
> `VZNATNetworkDeviceAttachment`, enforce egress with **nftables inside the
> guest**, and run agent tools as an unprivileged user (no `CAP_NET_ADMIN`) so
> they can't tamper with the ruleset. This aligns network with how the
> command-mode capability axis is already enforced (in-guest, by root
> `petri-guest`). The spike code (`petri-net`, the `VZFileHandle` attachment,
> the `--net-filter` wiring) has been reverted from the tree and parked in
> `git stash` ("spike/0036 smoltcp host-side egress filter") in case a future
> "survives guest-root compromise" high-assurance mode wants it back. See ADR
> 0002 for the revised design.

This is the de-risking spike before building the real `[policy.network]` axis. It
swaps Apple's in-framework NAT for a `VZFileHandleNetworkDeviceAttachment` and runs
a userspace gateway + egress filter ([`petri-net`](../../crates/petri-net)) on the
host side of the VM's L2 link.

## Exit criteria (from the issue)

1. **Allow/deny enforced host-side, per-IP.** From inside the guest, `curl` to an
   allowed IP succeeds and `curl` to a denied IP is blocked.
2. **Live-switchable without a VM restart.** Flipping the allowlist over the
   control socket changes guest reachability with the VM still running.
3. **Documented throughput delta** vs. the built-in `VZNATNetworkDeviceAttachment`
   baseline (e.g. `iperf3` or a large download).

If smoltcp throughput is unacceptable, fall back and repeat with the `libslirp`
crate before committing to a direction.

## What's wired

- `petri-vz` gains `--net-filter` (+ `--net-helper`, `--net-control-socket`,
  `--net-allow`, `--net-full`). With `--net-filter`, it creates a
  `socketpair(AF_UNIX, SOCK_DGRAM)`, hands one end to
  `VZFileHandleNetworkDeviceAttachment` and the other (dup'd to fd 3) to a spawned
  `petri-net`, which it kills on VM stop/teardown.
- The host backend turns this on when **`PETRI_NET_FILTER=1`** and the policy has
  `network_enabled = true`. It resolves `petri-net` as a sibling of the `petri`
  binary, puts the control socket at `<state-dir>/<id>/petri-net.sock`, and seeds
  the allowlist from `PETRI_NET_ALLOW` (comma-separated CIDRs) / `PETRI_NET_FULL=1`.
- This is **env-gated on purpose** — the spike does not yet add a
  `[policy.network]` schema. That lands with the production feature once the spike
  passes.

## Limitations (spike scope)

- **TCP + IP allowlist only.** No UDP forwarding, so in-guest **DNS won't
  resolve** — use IP literals in the tests. No SNI/Host/domain inspection.
- Denied flows are **dropped** (guest sees a connect timeout, not a reset). Use
  `curl --connect-timeout`.
- Single guest, fixed lease: gateway `192.168.127.1`, guest `192.168.127.2`.
- Naive 1ms poll loop; see throughput notes below.

## Running the spike

### 0. Build

```sh
swift build --package-path crates/petri-vz
codesign --force --sign - \
  --entitlements crates/petri-vz/petri-vz.entitlements \
  crates/petri-vz/.build/debug/petri-vz
cargo build -p petri -p petri-net      # petri-net lands beside petri in target/debug
```

Point the backend at the freshly built helper and net filter:

```sh
export PETRI_VZ_BIN="$PWD/crates/petri-vz/.build/debug/petri-vz"
export PETRI_NET_BIN="$PWD/target/debug/petri-net"   # optional; sibling resolution also works
export PETRI_NET_FILTER=1
export PETRI_NET_ALLOW="1.1.1.1/32"                   # initial allowlist
```

### 1. A network-enabled policy

The guest must be allowed to run `curl` (and `iperf3` for throughput). Network is
gated at boot by `network_enabled = true`. Example `spike-policy.toml`:

```toml
[policy]
network_enabled = true
max_runtime_secs = 120
max_output_bytes = 1048576
workspace_path = "/workspace"

[policy.command]
default = "yolo"
max = "yolo"
```

### 2. Create the sandbox

```sh
PETRI=$PWD/target/debug/petri
ID=$($PETRI sandbox create --workspace "$PWD" --policy "$PWD/spike-policy.toml")
echo "sandbox: $ID"
SOCK="$HOME/.petri/instances/$ID/petri-net.sock"   # or $PETRI_STATE_DIR/$ID/petri-net.sock
NET=$PWD/target/debug/petri-net
```

Watch `petri-vz` log `spawned petri-net egress filter (pid …)` and `petri-net` log
`gateway up …`. The guest should DHCP an address and bring its link up.

### 3. Criterion 1 — allow vs. deny (per-IP, host-side)

```sh
# allowed (seeded above): expect HTTP 200/301-ish, fast
$PETRI sandbox exec "$ID" -- curl -sS --connect-timeout 5 -o /dev/null -w '%{http_code}\n' https://1.1.1.1
# denied: expect a connect timeout / non-zero exit
$PETRI sandbox exec "$ID" -- curl -sS --connect-timeout 5 -o /dev/null -w '%{http_code}\n' https://8.8.8.8
```

`petri-net`'s stderr logs `ALLOW`/`BLOCK` per flow.

### 4. Criterion 2 — live switch (no restart)

```sh
$NET control "$SOCK" status                 # -> allowlist [1.1.1.1/32]
$NET control "$SOCK" "allow 8.8.8.8/32"     # flip, VM still running
$PETRI sandbox exec "$ID" -- curl -sS --connect-timeout 5 -o /dev/null -w '%{http_code}\n' https://8.8.8.8   # now succeeds
$NET control "$SOCK" "deny 1.1.1.1/32"
$PETRI sandbox exec "$ID" -- curl -sS --connect-timeout 5 -o /dev/null -w '%{http_code}\n' https://1.1.1.1   # now blocked
$NET control "$SOCK" full                   # all egress; or `none` to block all
```

### 5. Criterion 3 — throughput vs. NAT baseline

Run an `iperf3 -s` somewhere reachable by IP (or download a large file). Measure
under the filter, then re-create the sandbox **without** `PETRI_NET_FILTER=1` (NAT
baseline) and measure again.

```sh
# filter on (allow the iperf server IP first):
$NET control "$SOCK" "allow <iperf-server-ip>/32"
$PETRI sandbox exec "$ID" -- iperf3 -c <iperf-server-ip> -t 10
# baseline: unset PETRI_NET_FILTER, recreate, repeat.
```

## Results (2026-06-07, Apple Virtualization, aarch64)

Run on a freshly built spike image (`curl`/`iperf3`, no LSP) with the updated
guest. The guest base image ships no DHCP client, so the NIC (`enp0s1`) was
configured statically (`192.168.127.2/24` via `192.168.127.1` for the filter;
`192.168.64.2/24` via `192.168.64.1` for NAT). `petri-net`'s DHCP server is
therefore exercised by the unit tests, not this run. Download = 200 MiB file from
a host `python3 -m http.server` (host-local in both cases, so the only variable
is the network backend).

| Metric | NAT baseline (Apple in-framework) | petri-net filter (smoltcp) |
|---|---|---|
| Single-stream download | ~5.8 GB/s (host-local, ~46 Gbit/s) | ~43.7 MB/s (~350 Mbit/s) |
| 8 parallel streams (aggregate) | not measured | ~52 MB/s (~420 Mbit/s) |
| Allowed `curl https://1.1.1.1` | n/a | HTTP 301 in 0.12s |
| Denied `curl https://8.8.8.8` | n/a | RST, fails in <1ms (see note) |

Findings:

- **Criterion 1 (allow/deny per-IP host-side): ✅ pass.** `1.1.1.1` (allowed) →
  HTTP 301; `8.8.8.8` (denied) → connection refused. Filter logged
  `ALLOW … -> 1.1.1.1:443` and `BLOCK … -> 8.8.8.8:443`.
- **Criterion 2 (live switch, no restart): ✅ pass.** `allow 8.8.8.8` + `deny
  1.1.1.1` over the control socket flipped reachability on the running VM:
  `8.8.8.8` then returned HTTP 302, `1.1.1.1` then refused.
- **Criterion 3 (throughput): ⚠️ works, but slow.** ~43 MB/s single-stream vs
  multi-GB/s host-local NAT. Aggregate barely rises with 8 parallel streams
  (~52 MB/s), so this is **not** poll-latency/windowing bound — it's a
  single-threaded datapath ceiling (per-frame `recv`/`send` syscalls, a `Vec`
  allocation + copy per frame, and serial `iface.poll`). smoltcp itself claims
  ~Gbps, so the bottleneck is our naive I/O, not the netstack.
- **Decision: proceed with smoltcp, but optimize the datapath before calling it
  done.** Correctness is fully demonstrated; the throughput gap is in our frame
  I/O, which is fixable without changing netstack choice: batch with
  `recvmmsg`/`sendmmsg`, avoid per-frame heap allocation, and move host splicing
  off the poll thread (or make the loop event-driven). Re-measure after that
  before considering `libslirp`. ~400 Mbit/s aggregate is already adequate for
  package-pull workloads; heavy transfer workloads need the optimization.

Two observed deviations from the spike's stated assumptions, both improvements:
- **Denied flows get a RST, not a silent timeout.** With `any_ip` enabled,
  smoltcp accepts the SYN at the IP layer and — finding no listening socket — emits
  a RST, so `curl` fails in <1ms instead of waiting out the connect timeout. The
  doc/README text describing a "connect timeout" on deny is thus conservative; the
  real behaviour is a fast refusal.
- **`any_ip` needs a default route whose next-hop is one of our own addresses**
  (`add_default_ipv4_route(GW_IP)`), or smoltcp drops the SYN before it reaches a
  listener. This was found during testing and is in the code + comments.

## Notes for the host run

If single-stream throughput is poor, suspect the 1ms poll cadence in
`petri-net`'s loop (tune the `wait_readable` ceiling or switch to event-driven
wakeups) and re-measure with parallel streams (`iperf3 -P 8`) to separate
windowing latency from raw throughput. The host-side parts of the netstack (DHCP,
ARP, TCP termination, splice, live allowlist) are covered by `cargo test -p
petri-net`; what only a real VM exercises is the VZ file-handle framing and
end-to-end throughput.
