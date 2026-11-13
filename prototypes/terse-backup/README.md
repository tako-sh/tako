# Embedded terse backup prototype

Run from the repository root:

```sh
just test terse
cargo test --locked -p tako-storage
cargo clippy --locked --manifest-path prototypes/terse-backup/Cargo.toml --all-targets -- -D warnings
```

This is an isolated experiment, not a deployed Tako feature. It uses the vendored
`terse-litestream` 0.2.0 from `3a78cd6c8d70f1b607dbc10c4368b25eb6122731`;
the only upstream change is rusqlite 0.40.2 to 0.41. Both the prototype and patched
upstream tests lock rusqlite 0.41.0 and libsqlite3-sys 0.39.1, matching Tako's
lockfile. See `vendor/README.md` for provenance. The first build fetches Rust
dependencies, including the separately pinned terse-ltx repository.

Tests use the same fsync-enabled Apache `object_store` local backend as
production's `tako-storage` crate. No object-storage daemon, cloud credentials,
or bucket is required. The Rust storage-handler tests separately exercise SDK
URL signatures, streaming uploads/downloads, interrupted uploads, app isolation,
and migration from the former SDK-managed directories. The MinIO runner was
removed.

## Inexpensive defaults

`BackupLoop` captures and uploads together every **60 seconds**. Delaying uploads
alone would still produce a PUT per file if capture ran every second. The tests
verify that multiple writes inside the minute become one incremental object and
that idle ticks make zero remote requests. A large capture can span multiple
segments, so this is not a hard request or dollar cap.

Failed uploads back off at 1, 2, 4, 8 and then **15 minutes**, while local capture
continues every minute. Success resets the delay. The shared S3 backend disables
internal HTTP retries so the outer schedule controls attempts. Idle periods and
uploader restarts use local receipts, with no bucket listing or HEAD polling.
Remote listings happen only during an explicit restore. Retention verifies its
replacement anchor against the local spool.

History has **seven-day retention**, with cleanup on startup and once every
24 hours after successful uploads. In steady state, the daily interval can retain
almost eight days of changes. This is a time window, not a hard byte or dollar
cap: storage depends on database size and write volume. Outages delay cleanup.

Cleanup uses local LTX capture timestamps and durable upload receipts, without
remote LIST/HEAD requests or extra timestamp objects. It compacts the expired
prefix into one full restore anchor, verifies the anchor locally, uploads it
with create-only semantics, and only then deletes the covered remote objects,
local inputs and receipts. S3 deletions use the library's bulk-delete stream.
The anchor can be older than seven days and must remain: blindly expiring every
object with an S3 lifecycle rule would break the retained restore chain.

All capture points at or after the cutoff remain available. The last earlier
point survives as the base; older targets expire. A backwards capture clock
conservatively retains additional history. Idle databases keep one base without
repeated uploads or remote maintenance requests. Interrupted cleanup is safe to
retry, including already-deleted remote objects.

Before enabling continuous backups for users, require explicit database paths.
Scheduled snapshots can retain their existing automatic SQLite detection.

## Data flow

- `Capture` holds the SQLite connections and writes durable local LTX segments.
  Application writers use ordinary SQLite connections in separate processes.
- `upload_pending` scans local L0 files, compacted L9 anchors and local receipts. It uses create-only
  object writes, records a receipt only after success, and never lists or HEADs
  the remote store. A lost acknowledgment triggers one comparison GET on retry.
- `restore_remote` lists the remote prefix on explicit recovery, downloads the
  segments into fresh temporary storage, and asks terse to validate and publish
  the database. Historical targets are capture TXIDs, not wall-clock timestamps.

Capture is synchronous and belongs on a blocking worker. Uploads operate on a
cloned spool independently of the capture handle. The caller drives `BackupLoop`
with monotonic time; it owns no thread or timer. Each database needs a dedicated
spool and exclusive remote prefix. Local capture and upload locks prevent
duplicate owners of one spool; they do not coordinate different machines or
different spools watching the same database. Keep the destination fixed: local
receipts assume acknowledged remote objects remain present and immutable.

## Verification

The fast tests use a filesystem-backed `object_store`, with counted API operations and
failures injected before upload and after remote publication. They use real
writer and capture subprocesses, terminated with `Child::kill` (SIGKILL on Unix).
Restored databases must pass `PRAGMA integrity_check` and match every expected
row. Tests include concurrent writers with aggressive automatic checkpoints,
uncommitted writer death, capture death before upload, continued writes and
checkpointing while capture is down, historical recovery, lost acknowledgments,
conflicting/missing/corrupt objects, continued app writes during failed uploads,
and 100 idle capture/upload ticks with zero remote calls. Retention tests advance
LTX timestamps by re-encoding test captures, preserving checksums. They verify
all seven-day targets after repeated pruning, expired target rejection, idle
bases, failed anchor uploads, lost acknowledgments, and interrupted deletions.
Restores read remote objects alone; the rolling-retention test removes the source
and spool before checking every retained capture point.

The fixture binary is only a subprocess test driver. Tests create disposable
databases and object directories; they do not access deployed app data or cloud
credentials.

## Deliberate limits

No hosted S3 service, power-loss/torn-write simulation, soak test, timestamp index,
encryption, background task runner, or server/CLI config integration is included.
Retention bounds normal history by age rather than bytes; pending captures still
accumulate during outages. Explicit restores must be serialized against cleanup
before server integration; this prototype does not coordinate restore readers
with deletion. Local filesystem deletions are not power-loss durable, so an
unclean machine shutdown may leave expired remote files. One segment is buffered
for upload, and explicit restore downloads the
whole prefix. Large-database memory/restore costs need separate evaluation.
Capture reopen can generate a full snapshot; restarting capture per upload tick
would defeat incremental backup. This prototype must not establish a production
durability or PITR guarantee.
