# Benchmarks

The complete 27 October 2026 suite compares Tako, nginx, HAProxy, Envoy and Caddy,
plus Tako channel publish and workflow enqueue. All 135 cases are included:
three repetitions at seven HTTP and five feature concurrency levels.

- [Complete report and raw data](https://github.com/tako-sh/performance/tree/main/results/20261027-full-suite)
- [Current performance summary](https://github.com/tako-sh/performance/blob/main/RESULTS.md)
- [Testing and publication guide](commands/performance.md)

Tako source: `1093899fd6b09d6227e5b7b585ee8bfeb4b5c028`. The proxy, application,
load generator and sampler share two CPUs and an 8 GiB container limit on free
GitHub Actions runners. Backend descriptor limits are matched at 65,535; Tako's
production app default remains 4,096.

| Proxy | 200 RPS at c5000 | 200 RPS at c20000 | Peak PSS at c20000, MiB |
| --- | ---: | ---: | ---: |
| nginx | 8,467 | 5,989 | 496 |
| HAProxy | 8,406 | 4,764 | 1,048 |
| Envoy | 6,682 | 3,621 | 1,043 |
| Tako | 5,850 | 2,877 | 1,343 |
| Caddy | 3,240 | 1,359 | 1,373 |

Values are means of three per-run measurements. Tako beats Caddy in successful
throughput at every tested level; nginx, HAProxy and Envoy are faster than Tako.
Tako and HAProxy have no HTTP/client errors across their HTTP cases. Other
proxies have errors in some cases; see the full report before comparing capacity.
Tako's remaining memory usage is substantial and is not considered solved.

At c8000, channel publish averages 2,334 successful HTTP requests/s and workflow
enqueue 2,165. Channel runs are clean; one workflow repetition returned 483 HTTP
502s. Enqueue throughput measures HTTP acceptance, not workflow completion.

Two nginx c15000 runs have fewer resource samples than planned; one Tako c15000
load-generator CPU aggregate is unavailable because its process counter reset
at exit. Raw data and quality flags are retained. No failed repetitions are
removed. This hardware/setup differs from the historical VM, so changes in
absolute throughput do not establish a code regression.

The earlier controlled buffer-lifetime validation remains separate evidence for
keeping the merged memory fix. Local unaccepted allocator/TLS/body prototypes
are not used in this full-suite dataset.
