# Benchmarks

Detailed benchmark harness, raw data, graphs, methodology, VM details, and
analysis live in the dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest results: <https://github.com/tako-sh/performance/blob/main/RESULTS.md>

## TLDR

Latest clean single-VM HTTP/TLS run:

- Tako release: `tako-server 0.0.0-850a9e2`
- Setup: load generator, proxy, and app all on one 2 vCPU Ubuntu VM
- Result: Tako clearly beats Caddy in this setup, but still trails nginx in the
  lower-load clean rows
- At c2500, Tako is within about 8% of nginx on successful RPS and has slightly
  better p99 in this run
- c7500+ is overload behavior on this VM; CPU is saturated and p99 is already
  in seconds
- Main next target: reduce Tako proxy RSS and downstream connection/session
  memory pressure under high concurrency

Load-balanced mode is intentionally excluded from the exe-node result set. It
needs a larger or multi-node testbed.
