---
description: Run Tako performance experiments in GitHub Actions and publish validated results
---

$ARGUMENTS

# Performance Benchmark

Use [tako-sh/performance](https://github.com/tako-sh/performance) for benchmark
workflows, harnesses, patches used only for measurement, raw data, and reports.
Run experiments on free standard GitHub-hosted Linux runners in that public
repository by default. Do not require an SSH VM or add benchmark workflows to
`tako-sh/tako`. Use a dedicated VM when the workload needs isolation, additional
capacity, or multiple machines.

Carry the requested work through execution, inspection, necessary fixes, reruns,
and a reviewable result. A diagnostic run belongs in its own dated report; it
does not automatically replace the public benchmark or website charts. For a
publication request, update those summaries only after complete comparable runs
support the claims.

## Inputs and defaults

- Reuse the performance checkout available in the workspace, or clone
  `https://github.com/tako-sh/performance.git`. Inspect existing work before
  creating an isolated branch/worktree; preserve unrelated changes.
- For release comparisons, resolve the latest published Tako release to an exact
  revision. For optimization experiments, pin the unchanged baseline and
  candidate source, and describe the patch. Never silently substitute an older
  workflow pin for the requested build.
- Default to the standard `ubuntu-24.04` Actions runner. No paid larger runner or
  cloud machine without explicit authorization.
- Select the workflow for the question using the table below. Read its harness
  before running: these workflows pin source revisions and toolchains rather
  than accepting a Tako revision input.
- As of this guide's update, the memory and buffer workflows are in
  [performance PR #2](https://github.com/tako-sh/performance/pull/2), branch
  `codex/memory-benchmark`. Check its current status. After merge, use the
  performance repository's default branch or the experiment branch derived from
  it. Do not assume an unmerged workflow can be manually dispatched.
- The independent TLS experiment is stacked in
  [performance PR #3](https://github.com/tako-sh/performance/pull/3), branch
  `codex/tls-offload-investigation`. Keep its results separate from buffer changes.
- These diagnostics do not run the complete five-proxy report, channels,
  workflows, or multi-node load balancing. Extend missing coverage and label it
  accurately.
- Use HTTPS with the same certificate, route, upstream response, request timeouts,
  URL/Host/SNI, source IPs, concurrency, warmup, and measurement interval for
  every compared proxy. Keep the load generator's request timeout at 60 seconds
  and its per-host connection cap equal to concurrency.
- Keep only useful history, but preserve failed, incomplete, and superseded
  evidence that explains a correction. Do not select only favorable repetitions.

| Question | Workflow | Harness |
| --- | --- | --- |
| Fixed 64/32/16 KiB buffers, active/held connection memory | `memory.yml` | `diagnostics/full-memory/` |
| Release empty L4 buffers while preserving 64 KiB reads; release completed HTTP/1 upload buffers | `buffers.yml` | `diagnostics/buffer-lifetime/` |
| Handshake interference with established requests, after profiling finds evidence | `tls.yml` | `diagnostics/tls-offload/` |

All workflow paths are under `.github/workflows/` in the performance repository.

## Acceptance requirements

For memory optimizations, both comparisons must pass under equivalent conditions:

1. Preserve unchanged Tako's throughput, p95/p99 latency, and CPU efficiency per
   successful request or transferred byte. A repeatable regression blocks the
   change; no percentage of slowdown has been approved in exchange for RAM.
2. Keep Tako faster than Caddy on the compared workloads, with correct successful
   responses. Beating Caddy does not excuse slowing down relative to Tako.

Cover small requests, large uploads and downloads, HTTP/1.1 and HTTP/2, steady
connections and connection churn. Add long-lived streaming and idle/reactivation
cycles for changes to buffer lifetime. Check framing, pipelining, unread bytes,
flush behavior, cancellation, and connection reuse when changing buffering.

Use enough paired repetitions, reversing or balancing order, to quantify noise.
Compare tail latency at matched offered load as well as saturation throughput.
Verify that the client and upstream are not hiding proxy differences. Two short
runs or an inconclusive difference do not establish absence of a regression.
The current memory workflow is a diagnostic, not this full acceptance suite.

## Phase 1 — Preflight and build identity

1. Inspect both repositories' status, instructions, intended branches, and open
   benchmark PRs. Use isolated worktrees when needed.
2. Read the selected workflow, harness README, and recent diagnostic report.
   Confirm its scope matches the question before spending a benchmark run.
3. Pin and record the Tako revision, performance revision, dependency lockfiles,
   candidate patch/hash, Rust and Go toolchains, Caddy plus rate-limit plugin,
   runtime libraries, and container image digest. Capture binary hashes too.
   PR runs may check out a merge commit; record the actual measured checkout.
4. Validate affected harness code and the candidate before measuring. The memory
   workflow runs `cargo test --locked -p tako-server proxy:: -j 4` before the
   release build. Run affected Go tests, Python syntax checks, and shell checks
   when changing those parts.
5. Confirm the resource limits and measurement boundaries. The memory workflow
   uses a disposable container with **2 CPUs of quota, 8 GiB RAM, and no swap**;
   the proxy, load generator, and upstream app share that quota. This is not an
   exclusive two-core VM. Host contention and different runner CPUs can change
   absolute results between jobs.

The full-server harness uses Tako's original installer, Linux app identities and
cgroups, and a real deployed Go app. Keep that setup for product-level claims.
A minimal Pingora responder or fixed-upstream proxy is useful for attribution,
but must be labeled separately.

## Phase 2 — Run through GitHub Actions

Run these commands from the performance checkout. Pass `-R` explicitly so jobs
cannot accidentally run in the main Tako repository.

While the selected workflow exists only on a PR branch, push relevant harness/workflow
changes and use its `pull_request` run. Inspect the workflow's path filters; an
unrelated push may not trigger it. GitHub requires a workflow to exist on the
default branch for `workflow_dispatch`. Do not merge a PR just to trigger a test.

Once the workflow is available on the default branch, dispatch the intended ref:

```bash
gh workflow run memory.yml -R tako-sh/performance \
  --ref '<performance-branch>' -f stress=false
# For buffer lifetime or TLS experiments, choose the matching workflow instead:
gh workflow run buffers.yml -R tako-sh/performance --ref '<performance-branch>'
gh workflow run tls.yml -R tako-sh/performance --ref '<performance-branch>'
```

Find the run for the intended branch, event, and commit; never assume the newest
run belongs to this task. Inspect PR checks as well when using a PR trigger.

```bash
gh run list -R tako-sh/performance --branch '<performance-branch>' --limit 10 \
  --json databaseId,workflowName,event,headSha,status,conclusion,url
gh run view '<run-id>' -R tako-sh/performance \
  --json databaseId,workflowName,event,headSha,status,conclusion,jobs,url
gh run watch '<run-id>' -R tako-sh/performance --exit-status
```

The workflow cancels an older run on the same ref when a new run starts. A
cancelled run or a failed build is not completed benchmark evidence. For an
unchanged source/harness retry, `gh run rerun '<run-id>' -R tako-sh/performance`
replays the existing run; it does not pick up a newer branch commit.

Use `-f stress=true` with `memory.yml` only when investigating 20,000 continuously active workers.
That optional saturation test can overload the upstream and fail health checks.
It is different from holding 20,000 exercised connections open.

The workflow builds release binaries and runs inside a disposable privileged
container with private cgroup and network namespaces. Follow its Docker setup;
never mount the host cgroup tree into the benchmark container. Keep network and
file-descriptor limits equivalent across variants. The harness widens ephemeral
ports and backlogs, reserves listener ports, and uses fresh upstream ports where
needed to avoid source-port exhaustion and collisions during churn.

## Phase 3 — Collect and inspect the artifact

Result artifacts expire after 14 days. Their names depend on the workflow:

- `memory.yml`: `full-memory-<github.sha>`.
- `buffers.yml`: `buffer-results-<group>-<github.sha>` for `memory`, `h1`, `h2`.
  Its separate `buffer-binaries` build artifact expires after seven days.
- `tls.yml`: `tls-results-<github.sha>`.

Download promptly to a new directory; do not overwrite an earlier run.

```bash
gh run download '<run-id>' -R tako-sh/performance \
  --name '<artifact-name>' --dir 'results/<date>-<experiment>/ci-<run-id>'
python3 diagnostics/full-memory/summarize.py 'results/<date>-<experiment>/ci-<run-id>/active'
python3 diagnostics/full-memory/summarize.py 'results/<date>-<experiment>/ci-<run-id>/held'
```

Inspect the artifact layout before running the summarizer. If CLI artifact
download fails, use the GitHub UI or an available GitHub artifact tool. Verify
the run ID and artifact identity; never commit temporary signed download URLs.

Read `build.txt`, each mode's `metadata.json` and `summary.json`, and per-case
`load.json`, `samples.json`, and `summary.json`. The workflow uploads
JSON and build identity, including partial results when available. Runtime logs
are excluded because they can contain machine-specific source paths. Downloaded
artifacts and a rendered summary alone do not prove the run passed: check the job
conclusion, expected cases, repetitions, and every case's `success` field.

The fixed-buffer `memory.yml` matrix has four variants (Tako 64/32/16 KiB and Caddy), two
opposite-order repetitions, and two modes: eight active and eight held cases.

| Mode | Workload | Memory statistic | What it establishes |
| --- | --- | --- | --- |
| Active | 5,000 workers; 5 s warmup, 20 s nominal load; 13-byte responses over TLS HTTP/1.1 | Peak proxy-process PSS, including warmup and churn | Memory under this traffic and preliminary throughput/latency |
| Held | 20,000 TLS connections; 21 verified requests each, at most 64 in flight; hold open 10 s | Median proxy-process PSS during the holding phase | Retained memory at a verified connection count, not maximum RPS |
| Optional stress | 20,000 continuously active workers | Peak PSS plus errors, latency, and throughput | Saturation behavior; failures are not capacity wins |

The buffer-lifetime matrix uses a separate unchanged Tako binary and four
comparators: idle-buffer release, completed-body release, both, and Caddy. Three
repetitions reverse variant order on alternating runs. Expect 30 memory cases
(20,000 held connections and 2,000 completed uploads with responses held open),
90 HTTP/1 cases (small, fixed-rate, upload, chunked upload, download, churn), and
60 HTTP/2 cases (small, fixed-rate, upload, download). Each job is a separate
machine; compare candidates with their control in the same job. Use
`diagnostics/buffer-lifetime/summarize.py` on each downloaded group directory.

The TLS matrix compares unchanged Tako, one and two offload threads, and Caddy.
Expect 48 cases: four repetitions at 0, 1,500 and 2,500 new TLS connections/s
alongside 1,000 established-connection requests/s. Use
`diagnostics/tls-offload/summarize.py`. Inspect foreground p95/p99 and background
completed rate and duration together. Lower foreground latency accompanied by
less completed background work is not a win. CPU efficiency divides aligned
server CPU by both streams' successful requests, not only foreground requests.
This focused diagnostic does not establish peak-throughput or HTTP/2 acceptance.

Both newer harnesses verify payloads and negotiated HTTP version, synchronize
timed traffic with the memory/CPU sampler, and include arrival queueing in
fixed-rate latency. Profiling must be a separate run so profiler overhead does
not contaminate acceptance measurements.

For every case, inspect:

- Successful RPS, total RPS, p50/p95/p99, status counts, transport errors, response
  correctness, `error_kinds`, and `error_samples`. Request draining can extend the
  nominal duration; use the harness's recorded elapsed time.
- CPU and memory over time, including client/upstream utilization when judging
  bottlenecks. Do not divide CPU covering warmup and teardown by only timed
  requests and call it precise CPU per request; align the intervals.
- Proxy PSS, RSS, and private pages from `smaps_rollup`, sampled every 250 ms in
  this harness. These exclude client and upstream process memory. They are
  process-memory measurements, not Rust/Go heap measurements or total machine
  memory; kernel socket memory is not included in process PSS.
- Established connections on the TLS port, sampled about every five seconds.
  Active worker count is not a simultaneous connection count. Held cases must
  verify all 20,000 connections during the holding phase, before teardown.
  Deduplicate `/proc/net/tcp*` rows by socket identity: concurrent churn can
  repeat rows during the kernel's non-atomic scan. Preserve failed historical
  counts; do not retroactively mark them successful without the socket evidence.
- Idle retention and memory after close separately from active peaks. Lower
  post-burst memory does not establish lower memory while connections stay live.

Any non-200, transport error, incorrect body, or failed held-connection count
invalidates a successful-memory/capacity claim for that case. Preserve and
investigate it. Missing cases remain incomplete, even if completed cases look good.

## Phase 4 — Diagnose and rerun

Classify the finding before changing production behavior:

- **Capacity:** correct responses within an explicit latency/error SLO.
- **Saturation:** CPU/queues overloaded; useful for overload behavior, not an SLO
  capacity claim.
- **Harness limitation:** loadgen bottleneck, local source-port exhaustion,
  `TIME_WAIT`, listener-port collision, file-descriptor limits, or stale samplers.
- **Product behavior:** for example, upstream health checks timing out under
  shared CPU pressure and causing 502/503 responses. Do not relax production
  checks solely to turn a failed benchmark green.

Keep the 16 controlled loopback client IPs for high concurrency; otherwise Tako's
2,048-active-request per-IP default can become the bottleneck. Keep Caddy's
pinned `github.com/mholt/caddy-ratelimit` build and equivalent high-water guards
for other proxies. Report logging differences too: the current Tako experiment
retains its per-request app logging, while Caddy's access logging is off. These
are product-configuration comparisons, not isolated language-runtime comparisons.

Fix harness problems in the performance repo, validate them, and rerun all
comparisons affected by the correction. Put production fixes in Tako with the
appropriate tests. Preserve build identity and label patched builds explicitly.
Do not combine candidates from one environment or harness revision with controls
from another and present the difference as an optimization win.

### Buffer and Pingora findings to carry forward

Read the [full-server investigation](https://github.com/tako-sh/performance/blob/cf58e6278a9a618c36c0f33136a830dbb35fef21/results/20261026-full-memory/README.md)
and [harness guide](https://github.com/tako-sh/performance/blob/cf58e6278a9a618c36c0f33136a830dbb35fef21/diagnostics/full-memory/README.md)
for evidence and current limitations. Follow their default-branch successors
once merged instead of copying experimental numbers into this playbook.

- The tested baseline already uses Pingora 0.9. Its pooling, shutdown-contention,
  and long-timeout improvements are included. Check the actual lockfile and
  released source before crediting another upgrade or proposing a new setting.
- `ListenerConfig` / `L4BufferSettings` configure fixed accepted-socket buffers.
  The 64/32/16 KiB experiment changes ordinary HTTP/TLS listeners only; it does
  not change the custom PROXY-protocol listener, upstream buffers, or HTTP body
  buffers. No candidate has cleared the full performance acceptance requirement.
- Pingora 0.9 does not automatically resize or release these L4 buffers while
  idle. Dynamic retention is an experiment requiring buffer-layer changes and
  correctness checks; do not describe it as an available configuration switch.
- The buffer-lifetime prototype preserves 64 KiB reads and releases completed
  HTTP/1 body buffers separately. Its broader transfer coverage found existing
  Caddy advantages in some workloads. Follow the dated experiment report and
  do not generalize earlier small-response wins to every upload/download case.
- The earlier 64-to-4 KiB HTTP **body** buffer experiment was a different layer
  and did not establish a useful win. Do not confuse it with socket-buffer data.
- Smaller buffers can increase reads, syscalls, and CPU for large transfers.
  Small-response RPS alone cannot establish that uploads/downloads are unaffected.
- Keep equivalent connection reuse; disabling keepalive or closing connections
  after every request can hide retained memory while increasing handshake costs.
- `SSL_MODE_RELEASE_BUFFERS` is already enabled by the OpenSSL acceptor used
  here. `MIMALLOC_PURGE_DELAY=0` helped post-burst retention in earlier probes,
  not live peak memory. Neither is an established solution to live L4 allocation.
- Tokio 1.54 rejects the zero-capacity buffers created by Pingora 0.9; current
  Tako pins Tokio 1.53.1. Preserve the compatible lockfile and connection test
  until a tested upstream fix replaces that constraint.
- Separate released APIs from upstream proposals, and distinguish potential
  allocation savings from measured PSS. Do not infer all unexplained memory is
  TLS or GC overhead.

## Dedicated VM and broader benchmark fallback

Use a VM when the current Actions harness lacks the required workload or cannot
isolate the bottleneck. Reuse a host authorized for this task only after checking
it is still accessible and suitable. Ask for a host only if none is available
and the chosen test actually requires one; never assume a host from another chat.

Check VM load, OS, CPU/memory/swap, disk, and proxy **and app** file-descriptor
limits. Record provider/shape and sanitized metadata, not `uname -a` output
containing a real hostname. Verify the actual installed Tako binary/version.

The performance repo retains scripts for the five-proxy and channel/workflow
suites. Read their current inputs before use. Typical VM-local proxy invocation:

```bash
BENCH_VM='<ssh-host>' ./scripts/sync-to-vm.sh
BENCH_VM='<ssh-host>' \
TAKO_SERVER_BIN='<absolute-path-to-verified-binary>' \
SOURCE_IPS='127.0.0.2,127.0.0.3,127.0.0.4,127.0.0.5,127.0.0.6,127.0.0.7,127.0.0.8,127.0.0.9,127.0.0.10,127.0.0.11,127.0.0.12,127.0.0.13,127.0.0.14,127.0.0.15,127.0.0.16,127.0.0.17' \
PROXIES='nginx haproxy envoy tako caddy' MODES=single ENDPOINTS=plaintext \
CONCURRENCY_LIST='1000 2500 5000 7500 10000 15000 20000' \
WARMUP=10s DURATION=30s REQUEST_TIMEOUT=60s \
METRICS_INTERVAL=1 METRICS_CONNECTIONS=1 COOLDOWN_SECONDS=10 \
./scripts/run-vm-local-http-benchmarks.sh
```

Use paired/balanced order for optimization comparisons; do not leave a hot
previous case to bias the next proxy. High-load rows need separate lower-load,
matched-SLO checks. Load-balanced mode needs a larger or multi-node testbed;
skip it on the shared two-CPU setup.

Run channels/workflows separately with
`scripts/run-vm-local-tako-feature-benchmarks.sh`. Separate enqueue/publish
pressure from worker execution when diagnosing failures. Neither result is
provided by the current memory Actions workflow.

## Phase 5 — Report and publish only supported claims

For an experiment, add a concise `results/<date>-<experiment>/README.md` in the
performance repo with raw artifacts and these details:

- Question, baseline/candidate changes, exact build identity, Actions run URL/ID
  and attempt, and sanitized resource/environment metadata.
- Workloads, duration, repetition/order, measurement scope, per-run results and
  spread, successful-response counts, and remaining uncertainty.
- Failed/incomplete runs, harness corrections, and which cases supersede them.
- Whether memory improved, whether performance preservation is established, and
  how Tako compares with Caddy. State unmeasured workloads explicitly.

Only after complete comparable runs justify replacing the public report:

1. Update the performance repo's `RESULTS.md`, graphs, and raw-data links.
2. Update Tako's `BENCHMARKS.md` with a short TLDR and report links.
3. Update `website/src/data/performance.ts` and
   `website/src/pages/performance.astro` so numbers, hardware, versions,
   methodology, and claims agree with the report.
4. Update `website/src/pages/index.astro` if its performance claim changed.

Do not compare absolute Actions RPS against the old dedicated VM as if only the
code changed. Keep diagnostic results separate from published release results,
and never turn a failed or partial run into a headline win.

## Phase 6 — Sanitize, validate, and deliver

Before committing, inspect every changed report and raw artifact for real hosts,
IPs, usernames, absolute machine paths, peer/Tailscale identifiers, email
addresses, tokens, credentials, signed download URLs, and private runtime logs.
Controlled names such as `bench.test`, loopback addresses, generic container
paths, public revisions, and Actions run URLs are appropriate. Provider names
are fine without account or host identifiers.

Validate what changed: affected Go tests/builds, shell syntax, Python syntax,
Rust tests for a candidate, and website build when website files change. Run
`git diff --check` in each changed repository. Documentation-only edits need
link/command review, not a new performance run. Let required hooks finish; do not
commit known failures without explicit authorization.

Commit focused changes with Conventional Commit messages, push the working
branches, and create/update reviewable PRs. Keep harness/workflow/results changes
in the performance repo and production/summary changes in Tako. Preserve existing
work and do not push directly to default branches as part of this playbook.

Report what changed or ran, run/PR/report links, decisive numbers and their
limits, failed/incomplete checks, validation performed, and push status. For a
documentation-only request, state that no benchmark or public result changed.
