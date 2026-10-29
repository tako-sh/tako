# Tunnel transport benchmark

`concurrent_transport` exercises the real dev-server session reader, writer,
MessagePack encoding, local HTTPS forwarding, byte credit, and cancellation
ownership. A synthetic gateway consumes responses immediately and returns credit.
It does not measure browser rendering, the production relay/router, Cloudflare,
TLS on the public WebSocket, or geographic routing.

The harness starts 1 or 8 downloads, then issues twenty 32-byte probe requests at
50 ms intervals (at most four probes outstanding). It records download completion
and probe time to first body byte, including the local HTTPS request. Some probes
can complete after the downloads. `TUNNEL_BENCH_BODY_BYTES` and
`TUNNEL_BENCH_CHUNK_BYTES` control the local response size and TLS write size.

## Run

Build the ignored test with `cargo test -p tako-cli --bin tako-dev-server --no-run
--message-format=json`. Save the `executable` from the `compiler-artifact` record
for `tako-dev-server` before building another revision. For an unimpaired run:

```sh
TUNNEL_BENCH_CONCURRENCY=8 /path/to/test-binary concurrent_transport --ignored --nocapture
```

For paired measurements, use a **dedicated disposable Linux Docker container**
with `CAP_NET_ADMIN`, Python 3, iptables, and `libnetfilter-queue1`. Mount the test
binaries and this directory into it. Install the packages before disconnecting
its external network with `docker network disconnect bridge <container>`; all
benchmark traffic uses its loopback interface.

Run from the Docker host, substituting paths as seen inside the container:

```sh
python3 scripts/tunnel-benchmark/run.py \
  --container tako-tunnel-benchmark \
  --before /bench/before --after /bench/after \
  --impair /bench/impair.py --output results.json
```

The runner temporarily installs an owned `TAKO_BENCH` OUTPUT chain matching only
source/destination `127.0.0.2`, where the tunnel socket listens. The local HTTPS
app uses `127.0.0.1` and remains unimpaired. The runner removes its chain on exit.
Do not run concurrent benchmark jobs in the same container.

`impair.py` uses libnetfilter_queue to delay actual IP packets and randomly drop
them, leaving retransmission to the kernel TCP implementation. This works where
`tc netem` is unavailable. The profiles add 0 ms or 50 ms in each direction
(nominal 100 ms added RTT), with 0% or 1% random packet loss. Actual packet/drop
counts accompany each result. Loopback offload/coalescing means these are not
MTU-sized Internet packets, and configured 1% loss can produce zero drops in a
short run. Seeds repeat across variants, but packet sequences differ. Three
paired repetitions alternate before/after order; results are directional, not
production capacity estimates or statistical latency guarantees.

For the small-fragment workload:

```sh
python3 scripts/tunnel-benchmark/run.py \
  --container tako-tunnel-benchmark \
  --before /bench/before --after /bench/after \
  --impair /bench/impair.py --output small-results.json \
  --body-bytes 131072 --chunk-bytes 1024
```

The baseline for the recorded measurements is `4ed58f24` with this benchmark
module added. To reproduce it, create a detached worktree at that revision, copy
`tunnel_benchmark.rs`, add its `#[cfg(test)]` module declaration to `tunnel.rs`,
make the test helper `tunnel_state_with_app` visible with `pub(super)`, and remove
the `bytes` field from the harness's `ResponseCredit` message. No baseline
transport implementation changes are needed. The candidate uses protocol v2.
