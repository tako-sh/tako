# Runtime storage histories

Run the built-in seed corpus, or reproduce one workload:

```sh
just test histories
just test histories 42
```

Channel and workflow stores run against an in-process hosted database server
(`tako_db::testing::TestServer`), over the same HTTP protocol servers use.

Channel tests record invocation and completion order for concurrent appends and
paginated reads. A bounded checker searches for a sequential explanation that
respects real-time ordering. Negative checker tests reject reads that miss an
acknowledged append. Seeds reproduce workloads, not operating-system schedules;
failures include the complete observed history. Channel tests also reopen stores
while publishers are active.

Workflow tests drive duplicate enqueues, retries, lease expiry, stale-owner
writes, event signals/timeouts, and store reopenings. Every seed runs twice and
must produce the same logical trace. Timestamps and generated identifiers are
excluded from that trace. Lease expiry is injected into persisted test state so
these scenarios do not depend on sleeping until a deadline. Reclaim tests take a
run back within the same worker process and reject every mutation from the
previous claim. Leases follow the database server's clock, which tests check
with skewed server clocks. Concurrency tests run several stores against one
database: a claim takes exactly one due run, concurrent reclaimers and signals
consume state once, and apps sharing a database never see each other's runs.

These are focused storage tests. They do not simulate the private network, the
full async runtime, or external workflow side effects, and they do not prove
liveness.

## Local measurements

```sh
cargo run -p tako-channels --example notification_bench
cargo run -p tako-workflows --example sqlite_commit_bench
```

The Rust examples measure store operations against an in-process database host,
including its commits and local HTTP round trips. Channel measurements include
append, notification, and replay. They exclude TLS, the private network, and
browser delivery. Workflow measurements cover enqueue, claim, one checkpoint, and
completion.

Both commands use temporary local databases and print JSON. For the SQL driver
and commit benchmarks, see [BENCHMARKS.md](../BENCHMARKS.md). Compare revisions
on the same device with the same filesystem and build profile.
