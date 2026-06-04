# Benchmarks

Detailed benchmark harness, raw data, graphs, methodology, VM details, and
analysis live in the dedicated performance repository:

- Repository: <https://github.com/tako-sh/performance>
- Latest results: <https://github.com/tako-sh/performance/blob/main/RESULTS.md>

## TLDR

Latest clean single-VM HTTP/TLS run:

- Tako release: `tako-server 0.0.0-09b3dc6`
- Setup: load generator, proxy, and app all on one 2 vCPU Ubuntu VM
- Provider: exe.dev
- Result: nginx and HAProxy lead raw HTTPS reverse-proxy throughput; Tako
  clearly beats Caddy and Envoy in heavy rows and stays cleaner under overload
- Heavy rows: c5000 `12.5k` Tako 200 RPS vs `17.7k` nginx / `17.1k` HAProxy;
  c10000 `10.4k` vs `15.3k` / `14.8k`; c15000 `8.6k` vs `11.4k` / `13.2k`;
  c20000 `7.3k` vs `11.0k` / `11.2k`
- Tako stayed clean through c20000: 0 client errors and 0 non-200 responses.
  Envoy and Caddy overloaded in heavy rows; nginx showed small error/non-200
  rates at c15000; HAProxy stayed clean but had much worse p99 latency at high
  concurrency.
- Current master feature rerun: channels/workflows are clean through c8000.
  At c8000, channel publish reaches `4.6k` 200 RPS and workflow enqueue reaches
  `4.0k`, with 0 non-200 responses and 0 client errors.
- Keep RSS in the report. Follow-up controls show the high keepalive RSS is
  mostly Pingora/TLS reverse-proxy connection state, not a Tako-specific leak.
- Main next target: improve raw RPS and p99 latency versus nginx/HAProxy; Tako
  peaked around 2.7 GiB proxy RSS at c20000, but full Tako was only about
  14 MiB above a comparable fixed Pingora reverse proxy at 5k live keepalive
  connections.

Load-balanced mode is intentionally excluded from the exe-node result set. It
needs a larger or multi-node testbed.
