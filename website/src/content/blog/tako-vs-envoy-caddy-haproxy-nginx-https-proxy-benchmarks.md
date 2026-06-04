---
title: "Tako vs Envoy, Caddy, HAProxy, and Nginx: HTTPS Proxy Benchmarks on a Small VPS"
date: "2026-06-04T13:59"
description: "Raw HTTPS proxy benchmarks for Tako, nginx, HAProxy, Envoy, and Caddy on one small 2 vCPU VPS."
image: 7137c5fd0be8
---

We just published a public [Tako performance report](/performance/) with the data people usually ask for first: HTTPS throughput, p99 latency, CPU, memory, and clean-run behavior under heavy concurrency.

The short version is honest and useful: nginx and HAProxy are still the raw reverse-proxy throughput leaders on this small VPS. Tako does not match them yet. But Tako stays clean through the largest tested row, beats Envoy and Caddy in the heavy rows, and does that while running the app-aware proxy path that powers [`tako deploy`](/docs/deployment/), routing, instance selection, and source-IP handling.

This is the search-shaped version of the report: fewer graphs, more table, all caveats left in.

## The Benchmark Setup

The comparison used one small VM from [exe.dev](https://exe.dev): 2 vCPU, 7.8 GiB RAM, Ubuntu 24.04.4, Linux 6.12.90, KVM, AMD EPYC 9554P. The load generator, proxy, and app all ran on that same machine, so this is not a pure proxy microbenchmark. It is closer to a practical VPS question: "How much HTTPS traffic can this box produce end to end?"

Every proxy used the same route, self-signed TLS certificate, upstream app, and HTTP/1.1-over-TLS path. The route was `bench.test:18443`, resolved to loopback on the VM, with Host and SNI set to `bench.test`. Each row had a 10 second warmup and 30 second measurement window, with metrics sampled from `/proc` once per second.

| Item   | Value                                          |
| ------ | ---------------------------------------------- |
| VM     | 2 vCPU, 7.8 GiB RAM, no swap                   |
| Region | Tokyo, Japan                                   |
| OS     | Ubuntu 24.04.4, Linux 6.12.90                  |
| Path   | HTTP/1.1 over TLS                              |
| Timing | 10s warmup, 30s measured                       |
| Route  | `bench.test:18443` on loopback                 |
| App    | Same small Go plaintext app behind every proxy |

The software matrix was:

| Proxy   | Version in this run                               |
| ------- | ------------------------------------------------- |
| Tako    | `tako-server 0.0.0-09b3dc6`                       |
| nginx   | `nginx/1.24.0 (Ubuntu)`                           |
| HAProxy | `2.8.16-0ubuntu0.24.04.2`                         |
| Envoy   | `1.38.0`                                          |
| Caddy   | `v2.11.3` with `github.com/mholt/caddy-ratelimit` |

The raw report and row files are public in the [`tako-sh/performance`](https://github.com/tako-sh/performance) repo. The HTTP run lives at [`results/20260602T052009Z/http-vm-local`](https://github.com/tako-sh/performance/tree/main/results/20260602T052009Z/http-vm-local).

```d2
direction: right

load: "Load generator"
proxy: "nginx / HAProxy / Tako / Envoy / Caddy"
app: "same Go app"
metrics: "/proc sampler"

load -> proxy: "HTTPS, Host + SNI bench.test"
proxy -> app: "loopback upstream"
metrics -> load: "CPU + RSS"
metrics -> proxy: "CPU + RSS"
metrics -> app: "CPU + RSS"
```

## Throughput, p99, And Clean Runs

For raw 200-response throughput, nginx and HAProxy are the ceiling in this run. Tako lands below them, but well above Envoy and Caddy once concurrency gets heavy.

| Proxy   | c5000 200 RPS | c10000 200 RPS | c20000 200 RPS | c20000 p99 | c20000 clean-run behavior           |
| ------- | ------------: | -------------: | -------------: | ---------: | ----------------------------------- |
| nginx   |        17,698 |         15,309 |         10,991 |       3.8s | 0.00% non-200, 0 client errors      |
| HAProxy |        17,050 |         14,788 |         11,162 |      15.7s | 0.00% non-200, 0 client errors      |
| Tako    |        12,504 |         10,373 |          7,266 |      15.5s | 0.00% non-200, 0 client errors      |
| Envoy   |         4,735 |          3,664 |            828 |      26.6s | 40.70% non-200, 1.02% client errors |
| Caddy   |         5,174 |          1,705 |          1,271 |      26.4s | 0.00% non-200, 7.51% client errors  |

That table is the headline. At c5000 and above, Tako is not chasing nginx yet. It reaches about 66-75% of nginx's 200 RPS across c5000-c20000 and about 65-73% of HAProxy's 200 RPS. If your only question is "which static reverse proxy moves the most clean HTTPS responses on this tiny VM?", nginx and HAProxy win this run.

But clean-run behavior matters too. Envoy and Caddy both hit pressure in the heavy rows. Envoy starts recording client timeouts at c5000, then returns a large share of 503s at c15000 and c20000. Caddy starts returning 502s at c5000 and later records client timeouts. Tako stays all-200 with 0 client errors through c20000.

There is also a p99 story. nginx has the best c20000 p99 at 3.8s. HAProxy and Tako are roughly comparable at the largest row, around 15.5-15.7s. Envoy and Caddy are both above 26s. Under saturation, "how many requests completed?" and "how long did the slow tail wait?" need to be read together.

## CPU And Memory

The VM is saturated in the heavy rows. Total CPU is basically 100% for all proxies, so CPU by itself is not the differentiator. The useful questions are how that budget turns into clean responses, what tail latency looks like, and how much memory each proxy keeps while thousands of TLS connections are open.

| Proxy   | c20000 max CPU | c20000 proxy CPU | c20000 app CPU | c20000 loadgen CPU | c20000 proxy RSS | Max TLS conns |
| ------- | -------------: | ---------------: | -------------: | -----------------: | ---------------: | ------------: |
| nginx   |         100.0% |            40.8% |          18.0% |              41.2% |          262 MiB |        14,168 |
| HAProxy |         100.0% |            54.5% |          17.4% |              49.6% |          896 MiB |        20,012 |
| Tako    |          99.9% |            60.1% |          18.6% |              44.0% |        2,723 MiB |        20,396 |
| Envoy   |          99.9% |            99.4% |           7.0% |              72.6% |          999 MiB |        20,175 |
| Caddy   |         100.0% |            76.1% |           7.8% |              27.1% |        1,534 MiB |        20,000 |

Tako's RSS is the uncomfortable number, and we should keep it visible. The report includes controls: on the same VM at 5k live HTTPS keepalive connections, raw Tokio + OpenSSL used about 136.6 MiB RSS, a Pingora HTTPS responder used about 341.4 MiB, a fixed-upstream Pingora reverse proxy used about 535.3 MiB, and full Tako used about 549.0 MiB after deploy. Full Tako was only about 14 MiB above the comparable fixed Pingora reverse proxy.

So the current read is not "Tako has a simple routing leak." It is that Pingora/TLS reverse-proxy connection state is expensive in this high-keepalive shape, and Tako still needs tuning before we can call it nginx/HAProxy parity on raw proxy efficiency.

## Why Tako Is Doing More Than A Static Proxy

nginx and HAProxy are configured here as static reverse proxies. That is the right raw-throughput comparison, but it is not the same request path.

Tako still does product work on the proxy path:

| Request-path work               | Why Tako does it                                                     |
| ------------------------------- | -------------------------------------------------------------------- |
| App route lookup                | Routes come from app environments in [`tako.toml`](/docs/tako-toml/) |
| Source-IP derivation            | Supports direct, Cloudflare, and trusted-proxy modes                 |
| Per-client limiter accounting   | Protects deployed apps from one derived client IP                    |
| App and instance selection      | Connects routing to Tako's app lifecycle                             |
| In-flight accounting            | Supports draining, scale decisions, and healthy instance use         |
| Forwarding header normalization | Keeps app-visible request metadata consistent                        |

That is the point of using Tako instead of adding a proxy config file next to a deploy script. [`tako-server`](/docs/how-tako-works/) owns routing, TLS, app processes, readiness, scale, and rolling deploy state together. A request can find the deployed app, pick a healthy loopback instance, and participate in the same state model that deploys and logs use.

This is also why the benchmark result is encouraging without being finished. Tako stayed clean under c20000 while doing that app-aware work, but it still trails nginx and HAProxy on raw RPS and nginx on p99. The report calls out the next targets: larger-VM runs, external same-region load generation, Pingora session and upstream tuning, and precomputed per-instance upstream/header state.

The report also includes a current-master feature rerun for durable channels and workflows. Those paths are not proxy-only: the app uses the JavaScript [SDK](/blog/why-tako-ships-an-sdk/), publishes a channel message, or enqueues a workflow with one persisted `ctx.run("ack", ...)` step. Both paths stay clean through c8000: channel publish reaches 4,595 200 RPS with 6.4s p99, and workflow enqueue reaches 4,001 200 RPS with 7.7s p99.

## What To Take Away

If you need the highest raw HTTPS reverse-proxy throughput on a small VPS, this run says nginx and HAProxy are still the references to beat. They are excellent tools, and the numbers show why people trust them.

If you want the deployment tool to own routing, TLS, process lifecycle, readiness, scale-to-zero, secrets, and app-aware request handling, the Tako result is the useful one: 12.5k clean 200 RPS at c5000 and 7.3k clean 200 RPS at c20000 on a 2 vCPU VM, with zero non-200 responses and zero client errors in every Tako HTTP row.

That is not the finish line. It is a clear baseline, in public, with the rough edges left in. Read the full [performance page](/performance/), inspect the [raw benchmark repo](https://github.com/tako-sh/performance), or start from the [Tako docs](/docs/) if you want to see how the deploy and proxy pieces fit together.
