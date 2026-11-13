# terse-litestream

An embeddable Rust port of Litestream's SQLite replication core, targeting upstream
[v0.5.17](https://github.com/benbjohnson/litestream/tree/v0.5.17)
(`ccd326c175b583b5e82893a6078f06dcef5fba3f`).

Captures committed SQLite WAL pages, coordinates checkpoints, publishes LTX files,
replicates them through an injected store, compacts them, and restores databases at
an available transaction position. It runs in the host process; production use has
no Go binary, control socket, subprocess, or background thread requirement.

## Use

```rust,no_run
use terse_litestream::{Database, FileStore, Options, replicate, restore};

fn main() -> anyhow::Result<()> {
    let replica = FileStore::new("backups/account");
    let mut database = Database::open("account.sqlite", Options::default())?;

    // After the application commits its SQLite transaction:
    let captured = database.sync()?;
    let durable = replicate(database.store(), &replica)?;
    assert!(durable >= captured.txid);

    restore(&replica, "recovered.sqlite", Some(durable))?;
    Ok(())
}
```

`Database::with_store` injects local staging storage. Implement `ReplicaStore` and
`SegmentWriter` to supply another storage backend. `FileStore` is the included
filesystem implementation. LTX encoding and streaming compaction use the separately
pinned [terse-ltx](https://github.com/TerseAI/terse-ltx) library.

## Contracts

- Run synchronous calls on a blocking worker. A `Database` owns two SQLite
  connections and a persistent database descriptor. SQLite writers may use their
  own connections. The caller must enforce one capture owner per database and
  exclusive ownership of each replica prefix, including across processes/hosts.
- `sync()` durably captures locally; it does **not** acknowledge remote durability.
  Gate application output on successful `replicate()` reaching the captured TXID.
  Retain local files until required destinations acknowledge them. `close()`
  performs a final local sync; dropping a database does not flush replication.
- `SegmentWriter::commit()` must publish a complete, immutable file atomically and
  durably. Dropping an uncommitted writer must leave no listed segment. Listings
  must include acknowledged files. `FileStore` fsyncs files and directories and
  refuses replacement. An ambiguous local publication fences capture until reopen.
- Restore requires a complete chain from transaction one. It verifies LTX file
  checksums, checks transaction/database checksums when present, and publishes the
  completed database without replacing an existing destination. Treat replica
  contents as trusted backup data, not an unrestricted hostile-file upload API.
- After restoring onto a host with no local capture history, call
  `recover_position(&replica)` **before the first sync**. This seeds the old replica
  position and forces a full capture so subsequent transactions advance past it.
  The restored database must belong to that replica's logical database.
- To continue a replica from a fresh `restore(.., Some(txid))` without a full
  capture, open it with `Database::resume(path, options, local, txid)` before any
  connection writes to it. Its next commit becomes an L0 delta at `txid + 1`. It
  refuses a WAL holding frames but cannot detect other changes to the file.
- Application code must not independently delete/truncate WAL files, replace an
  open database, change page size, or run another replication/checkpoint owner.
  `_litestream_seq` and `_litestream_lock` are reserved control tables.
- Read locks protect the WAL during capture. Automatic checkpoints try PASSIVE
  first and use TRUNCATE when the configured bound cannot be relieved. External
  long-lived transactions can block checkpoints; callers must handle errors.

`snapshot()` captures a complete L9 image at a new TXID. After that image is
replicated and acknowledged by consumers, older local L0 files and snapshots can
be removed through `ReplicaStore::remove`. Keep the newest acknowledged snapshot
and all later captures so recovery remains possible.

The filesystem backend targets local Unix filesystems supporting SQLite locking,
atomic publication and directory fsync. Power-loss behavior still depends on the
filesystem and storage device honoring these operations.

## Compatibility and scope

The storage layout is `ltx/<level>/<min-txid>-<max-txid>.ltx`, with hexadecimal
16-digit transaction IDs, L0 captures and L9 snapshots. Default local metadata is
`.<database-name>-litestream`, matching upstream. All eight SQLite page sizes
(512 through 65536 bytes), WAL checksum byte orders, committed transaction
boundaries, page growth/shrink, schema changes and checkpoint modes are covered.

This is a port of the embedded replication core, not the entire upstream daemon.
The caller supplies scheduling, retries, cancellation between synchronous calls,
retention policy, ownership/fencing, metrics and cloud store implementations.
There is no upstream YAML/CLI/socket API, live `restore -follow`, timestamp target
selection, or v0.3 generation-directory restore. PIT restore accepts an existing
TXID boundary; compaction/retention can remove earlier restore points.

Intentional implementation differences:

- Reopening capture conservatively emits a full snapshot at the next TXID. Normal
  idle syncs emit nothing, and safe passive-checkpoint WAL reuse stays incremental.
- Explicit FULL/RESTART/TRUNCATE checkpoints capture a full image afterward to
  cover writes racing with checkpoint lock acquisition. This may add a TXID.
- `max_sync_wal_bytes` bounds each LTX batch at commit boundaries; a single
  transaction can exceed it and `sync()` drains the backlog before returning.
- LTX files have upstream's `NO_CHECKSUM` database-checksum flag while retaining
  their file checksum. Restore also supports checksummed LTX chains.
- Local publication errors require reopening to reconcile the last published
  transaction. Pre-publication write failures can be retried on the same handle.

TXIDs represent capture batches, not individual SQL transactions. Do not assume
identical TXIDs, compressed bytes, or checkpoint timings between implementations.
Compatibility is established by cross-restoring actual backups and checking the
SQLite database and application data.

## Validation

Requires Rust 1.89+, Go (tested with 1.27.1), Python 3, Git and a C compiler.

```sh
cargo test --locked
scripts/verify.sh
```

The full gate checks formatting and Clippy, runs release-mode Rust tests against
the original Go executable, and runs the selected upstream WAL, sync, snapshot,
checkpoint, restore and page-growth regressions with Go's race detector. It fetches
SHA-256-verified official binaries and verifies the upstream source commit.
`LITESTREAM_BINARY` and `LITESTREAM_SOURCE` can point at existing installations;
the binary version and source revision are checked. Download caches are ignored.
The same gate runs on Linux and macOS in `.github/workflows/compatibility.yml`.

Coverage includes:

- Rust → Go and Go → Rust restore, with byte-identical SQLite output and SQL
  integrity/content checks; all page sizes on Rust output and three on Go output.
- Alternating Go/Rust capture ownership using the same local metadata.
- Historical TXID restore, cross-version compaction, snapshots and L0 retention.
- 480 deterministic randomized operations with updates, rollback, VACUUM and
  checkpoints; concurrent writers and blocked checkpoints.
- Upstream WAL fixtures, every truncated prefix of the valid fixture, both WAL
  checksum byte orders, incomplete transactions and previous-frame validation.
- Injected write/publication failures, immutable segments, corrupt files, gaps,
  false database checksums and position recovery after restoring older data.
- Real process kills before and after segment publication, followed by restore
  and resumed capture (six crash cases per run).

This validation does not establish production CPU savings or replace a workload
soak test. Integration into the actor runtime and production rollout are separate.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE) for upstream provenance,
ported algorithms, fixture attribution and implementation differences.
