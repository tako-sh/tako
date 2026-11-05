# Runtime storage histories

Run the built-in seed corpus, or reproduce one workload:

```sh
just test histories
just test histories 42
```

Set `TAKO_TEST_POSTGRES_URL` to a disposable Postgres database to also exercise
concurrent channel histories against independent server connections. The Rust CI
job runs the Postgres tests with a temporary database.

Channel tests record invocation and completion order for concurrent appends and
paginated reads. A bounded checker searches for a sequential explanation that
respects real-time ordering. Negative checker tests reject reads that miss an
acknowledged append. Seeds reproduce workloads, not operating-system schedules;
failures include the complete observed history.

Postgres channel tests also reopen stores while publishers are active and
upgrade an unversioned schema. Migration tests append a new migration, verify
that it runs once, and check that a failed migration rolls back its schema
changes and version before a successful retry.

Workflow tests drive duplicate enqueues, retries, lease expiry, stale-owner
writes, event signals/timeouts, and database reopenings. Every seed runs twice and
must produce the same logical trace. Timestamps and generated identifiers are
excluded from that trace. Lease expiry is injected into persisted test state so
these scenarios do not depend on sleeping until a deadline. SQLite and Postgres
RPC tests also reclaim a run within the same worker process and reject every
mutation from the previous claim. A Postgres test gates checkpoint insertion
inside a trigger and verifies that ownership transfer waits for its transaction.
Separate-connection reclaim tests on both databases verify that a worker at its
concurrency cap recovers when another server reclaims its lease. Slot accounting
tracks individual claims, so a delayed release cannot free a replacement claim's
slot; concurrent admission and failed-claim tests cover the refresh path.

SQLite contention tests use independent connections to a file-backed database
with `synchronous=FULL`. A controlled writer gate checks that event signaling
acquires the write lock before reading waiters, then sees the other writer's
committed waiter. A concurrent lifecycle workload checks persisted results under
repeated writer contention. These tests cover the snapshot-upgrade race and a
bounded workload; SQLite still permits one writer at a time, and a writer that
cannot acquire the lock within the five-second busy timeout can return an error.

Run the SQLite contention tests directly with:

```sh
cargo test --locked -p tako-workflows --lib enqueue::sqlite::tests
```

A subprocess exits without closing its workflow database after acknowledging a
checkpoint. Recovery must retain the checkpoint and reject the old owner after
reclaim. This tests process-crash recovery. The `synchronous=FULL` connection
assertion verifies the host-crash durability setting; a process test cannot prove
physical disk behavior under power loss.

These are focused storage tests. They do not simulate networking, the full async
runtime, or external workflow side effects, and they do not prove liveness.

## Local measurements

```sh
cargo run -p tako-channels --example notification_bench
cargo run -p tako-workflows --example sqlite_commit_bench
python3 test-support/sqlite-commit-bench.py
```

The Rust examples measure actual local store operations. Channel measurements
include append, notification, and replay, with same-store and separate-connection
writers. They exclude HTTP, TLS, and browser delivery. Workflow measurements cover
enqueue, claim, one checkpoint, and completion.

The Python experiment isolates SQLite transaction amortization with 256-byte
records. It excludes batch-formation latency, application queries, admission
control, and fan-in scheduling; it is not an application throughput benchmark.
Measure those costs before adopting a batched writer. All three commands use
temporary local databases and print JSON. Results depend on the filesystem,
device, build profile, and host load; compare revisions on the same machine.
