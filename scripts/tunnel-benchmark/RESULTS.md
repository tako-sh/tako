# Tunnel protocol v2 measurements — 2026-10-29

The byte window fixes sensitivity to tiny response fragments and gives a fixed
payload budget. It is **not a general latency improvement**: faster producers
can queue more data ahead of interactive responses on the shared TCP connection.

The selected window is 128 KiB per response, with a 1 KiB minimum charge per
chunk and 64 KiB maximum chunks. Across 32 active responses that permits at most
4 MiB of credited body payload (excluding socket buffers and other queues),
compared with the previous maximum of 16 MiB. This supports up to 128 tiny chunks
without a credit round trip, instead of eight. Neither side batches SSE events.

Exploratory 2 MiB and 256 KiB windows increased concurrent probe delays; 64 KiB
restricted larger-fragment download throughput. The selected window balances
those effects. Tail latency under concurrent transfers remains an unresolved
transport limitation; QUIC or a separate latency-sensitive connection would
need their own measurements before adoption.

## Method

Baseline: `tako-sh/tako@4ed58f24`, with the benchmark harness added and no transport
changes. Candidate: the accompanying v2 implementation. Both used Rust 1.99,
the unoptimized test profile, and the same managed Linux host. The public
WebSocket is plain TCP inside the fixture; the local app serves HTTPS.

See [reproduction instructions](README.md) and [all 108 measured runs](results.csv).
Each case has three paired repetitions, with alternating variant order. Figures
below are medians of per-run completion times and per-run probe p95s, not pooled
percentiles. These small samples are directional observations.

The packet helper adds 50 ms each way to the tunnel socket and optionally drops
1% of queued IP packets. Kernel TCP performs real retransmission. Loopback
coalescing/offload changes packet sizes; this is not an Internet-MTU capacity
benchmark. The CSV includes observed packet/drop counts, including short 1%-loss
runs that happened to drop no packets. Seeds repeat but sequences differ across
variants. The synthetic gateway consumes immediately; production browser,
Cloudflare, TLS on the tunnel socket, and relay routing are excluded.

Twenty 32-byte probes start at 50 ms intervals, at most four outstanding. Some
finish after the downloads. Workloads use 1 KiB TLS writes into 128 KiB responses,
8 KiB writes into 2 MiB responses, or 64 KiB writes into 1 MiB responses. A TLS
write is not necessarily one tunnel chunk.

## Observations

| Local write / response | Downloads | Added RTT / loss | Completion, before → after | Probe p95, before → after |
| --- | ---: | --- | ---: | ---: |
| 1 KiB / 128 KiB | 1 | 100 ms / 0% | 2.15 s → 0.34 s | 145 ms → 237 ms |
| 1 KiB / 128 KiB | 8 | 100 ms / 0% | 2.20 s → 0.94 s | 155 ms → 790 ms |
| 1 KiB / 128 KiB | 1 | 100 ms / 1% | 2.21 s → 0.29 s | 155 ms → 240 ms |
| 1 KiB / 128 KiB | 8 | 100 ms / 1% | 2.26 s → 0.90 s | 188 ms → 794 ms |
| 8 KiB / 2 MiB | 1 | 100 ms / 0% | 4.33 s → 2.18 s | 155 ms → 190 ms |
| 8 KiB / 2 MiB | 8 | 100 ms / 0% | 5.99 s → 6.86 s | 423 ms → 652 ms |
| 8 KiB / 2 MiB | 1 | 100 ms / 1% | 4.44 s → 2.37 s | 158 ms → 192 ms |
| 8 KiB / 2 MiB | 8 | 100 ms / 1% | 6.41 s → 4.18 s | 400 ms → 642 ms |
| 64 KiB / 1 MiB | 1 | 100 ms / 0% | 1.21 s → 1.21 s | 190 ms → 189 ms |
| 64 KiB / 1 MiB | 8 | 100 ms / 0% | 2.68 s → 2.41 s | 645 ms → 643 ms |
| 64 KiB / 1 MiB | 1 | 100 ms / 1% | 1.31 s → 1.25 s | 201 ms → 194 ms |
| 64 KiB / 1 MiB | 8 | 100 ms / 1% | 2.56 s → 2.40 s | 649 ms → 640 ms |

Small-fragment responses complete substantially faster, but their concurrent
probes can have worse tail latency. The 8 KiB concurrent no-loss case also
regressed; the corresponding lossy case improved throughput while probe p95
increased. Larger-write results are closer to the old transport. These mixed
results argue against promising universally faster tunnels or raising the
window further without workload-specific evidence.

## Validation and rollout

- Relay: 48 tests; `cargo fmt` and Clippy across all targets/features pass.
- Dev server: 171 tests; formatting and targeted Clippy pass. The manual benchmark
  is ignored by the normal test suite.
- CLI: `just test cli` passes 61 tests, with seven platform-specific skips.
- `just test e2e` is blocked before tests by Debian bullseye-security package
  downloads returning 404 in the existing builder image.
- Website check is blocked by the existing `website/scripts/verify.ts:206`
  TypeScript TS7022 error; rendered verification has no built `dist/index.html`.

Deploy the relay and matching dev server together: the subprotocol is now
`tako-tunnel-v2`; older clients receive HTTP 426. Deployment was not performed.
