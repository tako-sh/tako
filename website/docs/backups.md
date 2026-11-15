---
title: "Backups"
description: "Continuously back up selected SQLite databases, share data between apps on a server, and create or restore encrypted manual app-directory archives."
---

Tako automatically backs up the SQLite databases you explicitly select in `tako.toml`. Full app-directory archives are manual: deploys and timers no longer create them. Use [release rollback](/operations/#logs-and-releases) to recover code and a backup to recover data.

## Configure a destination

```toml
[envs.production]
backup = { storage = "private_backups", databases = ["app.sqlite"], shared_databases = ["accounts.sqlite"] }

[storages.private_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://s3.example.com"
region = "auto"
```

```bash
tako storages credentials private_backups --env production
tako deploy --env production
```

`databases` contains normalized paths relative to `TAKO_DATA_DIR`. `shared_databases` contains paths relative to `TAKO_SHARED_DATA_DIR`. Use database filenames, not `-wal`, `-shm`, or `-journal` files. Absolute paths, traversal, symlinks, hard links, and duplicate paths are refused. Each list supports up to 64 databases. An empty list selects none; Tako does not discover databases automatically.

When upgrading from scheduled backups, add these lists to keep SQLite backups automatic. An existing `backup = { storage = "private_backups" }` configuration now enables manual app-directory archives only. Scheduled and post-deploy archives stop; their existing history remains downloadable and restorable.

The destination must be private S3-compatible storage without `public_base_url`. A backup-only storage resource is not exposed through `tako.storages` unless separately bound to the app. Keep the environment-managed backup keys and storage credentials: the server cannot recover encrypted backups without them.

## Know the backup boundary

`TAKO_DATA_DIR` remains private to an app/environment. `TAKO_SHARED_DATA_DIR` is shared by apps on one server, including different environments on that server. Each server belongs to one network. This directory is local disk, not a network filesystem or a way to share SQLite across servers. Use subdirectories to distinguish unrelated datasets.

Apps retain separate Unix users. Tako grants shared-directory access through their common group and pre-creates configured database files with group-write permissions. Shared directories use inherited POSIX default ACLs so newly created files, including SQLite WAL and shared-memory files, are group-writable even with a restrictive process umask. The server data filesystem must support POSIX ACLs. An app can still request an explicitly restrictive file mode, such as `0600`; such a file is not shared. Never put app-private secrets in the shared directory.

SQLite supports multiple local processes. Use WAL mode, short write transactions, and a busy timeout; SQLite still permits only one writer at a time. Coordinate schema migrations between the apps.

| Data | Protection |
| --- | --- |
| Selected app SQLite databases | Automatic encrypted WAL capture and upload |
| Selected shared SQLite databases | One automatic backup stream per database on the server |
| Other files under app `data/app/` and `data/tako/` | Manual full-directory archive |
| Tako workflow state | Included in manual full-directory archives |
| Arbitrary files in the shared directory | Not included in an app's full-directory archive; copy them separately during coordinated downtime |
| Compose named volumes, external databases, S3 contents | Protect separately |
| Network membership, shared secrets, device keys | Protect separately |

Manual archives exclude transient channel/cache databases and retain symlinks as symlinks. They do not follow a symlink to include shared data.

## Know what is captured when

Tako captures and uploads committed SQLite changes about once per minute. Idle databases generate no new upload traffic. Failed uploads back off from one minute to at most fifteen minutes while local capture continues. Check status for errors and the last uploaded TXID: local capture alone does not protect against losing the server.

A database has one capture worker, local spool, encrypted remote history, and retention policy. Two apps selecting the same shared path reuse that stream. They must specify the same destination; conflicting buckets, endpoints, regions, or addressing modes are rejected before deploy. Removing one declaration leaves capture running for the remaining apps. Removing the last declaration stops capture and preserves the existing history. Once registration completes successfully, each declaring app's backup key can recover the shared database's encryption key. Check `backups status` for registration errors before relying on a newly added key.

Remote SQLite history is scoped to the network and exact server name. Servers with the same name in different networks can use the same bucket without sharing a replica.

Recovery points are capture TXIDs, not SQL transaction IDs or arbitrary timestamps. Tako retains seven days of capture points and a base sufficient to restore them. Cleanup runs daily, so retained history can approach eight days. Upload times are publication times, not precise database transaction times. Retention runs only while the database remains configured.

The spool is private server data outside app directories. A filesystem lease prevents a reload or another server process on the same data directory from starting a second backup stream. Capture runs in a separate confined process so ordinary backup readers cannot release its SQLite locks.

## Create and inspect

```bash
# Manual full app-directory archive
tako backups now --env production --server la
# Manual archives, including database snapshots created by older servers
tako backups list --env production --server la --json
# Continuous capture/upload status
tako backups status --env production --json
# Recovery points for one selected database
tako backups list --database app.sqlite --env production --server la
tako backups list --database accounts.sqlite --shared --env production --server la
```

SQLite status includes `database`, `shared`, `replica_id`, `consumers`, `active`, `captured_txid`, `uploaded_txid`, `uploaded_at_unix_secs`, and `error`. Archive status reports the last manual full archive and its retention. Full archives retain 30 days by default; their cleanup runs when an archive is created, not on a timer.

A deployment grant can operate the app's already-provisioned private backups. Shared database restores require a server administrator because they interrupt every app on the server.

Download a manual archive with `tako backups download <backup-id> --env production --server la`. The result is an encrypted zstd-compressed tar archive. Continuous SQLite history is restored through the database commands below, rather than downloaded as a tar archive.

## Restore data

```bash
# Restore the newest uploaded capture
tako backups restore --database app.sqlite --latest --env production --server la
# Restore an explicitly selected capture
tako backups restore --database app.sqlite --txid 42 --env production --server la
# Restore a shared database (pauses all apps on this server)
tako backups restore --database accounts.sqlite --shared --latest --env production --server la
# Restore a manual full-directory archive
tako backups restore <backup-id> --env production --server la
```

Choose the server explicitly when the environment has multiple servers. Restore downloads and verifies the selected history before replacing live data. A private database restore stops the owning app and all its workflow workers, removes stale SQLite sidecars, swaps the database, and restarts the app. Other files remain unchanged. If restart fails, Tako attempts to stop the failed runtime and put the original files back.

A shared database restore pauses **all apps on that server**. Backup declarations are not an access-control list: an undeclared app may open the shared file later, so an open-file scan cannot establish a safe list of consumers. Tako persists the server drain intent, stops consumers, closes capture, holds the database's exclusive lease, swaps the database, and resumes the server. A crash or incomplete cleanup leaves the server drained rather than allowing writes into a partial restore. The original files are retained until restart succeeds.

A standby process using the same data directory must be stopped before restoring. Tako refuses the restore while it holds the standby lease. Restore also excludes uploads and retention for the selected database. After restoring an older point, capture starts a new snapshot above the previous highest TXID, preserving later history rather than overwriting it.

Manual full-directory restores replace the app's `data/app/` and `data/tako/` together. This can rewind workflow state. They pause that app's SQLite capture while files are replaced. Existing `databases` archives from older Tako versions remain restorable; new scheduled database archives are no longer produced.

## Resolve an unfinished restore

A restore Tako did not finish leaves `apps/<app>/<env>/.data-restore-prev-<id>/` or
`.data-restore-abandoned-<id>/` beside the app's `data/` directory, below the Tako
data directory (`/var/lib/tako` by default). The directory holds the original data
and a record, `restore.json`, whose `state` says what the live data is:

- `"original"`: the original data was put back in full. Nothing to do beyond
  removing the directory.
- `"restored"`: the restore completed and only its cleanup or the restart failed.
  The live data is the restored data. Nothing to do beyond removing the directory.
- `"swapping"`, or a record Tako cannot read: the live data may be partly
  restored, with some databases restored and others original. Tako keeps the app
  stopped: server starts do not start it or its workflows, a running standby
  stops it within seconds, requests do not cold start it, `tako status` shows the
  reason as its error, and deploys, release commands, `tako scale`, secret
  updates, and `tako backups now` are refused with the same message. Continuous SQLite capture pauses while the app is blocked. A manual backup is refused.

A directory without `restore.json` means the live data is whole, either the
original or the restored data; remove it.

Shared database restores leave these directories below `shared/`, beside
`shared/data/`. The same states and database recovery steps apply, with every app
kept stopped while repairing shared data. Once the files and restore record are
resolved, use `tako network server resume <server>` to clear the persisted drain
and restart the apps.

To go back to the original data after a partial restore, with the app stopped,
work as the user that owns the app data:

1. For a **database snapshot** restore, the directory holds each original database
   at its path below `data/` (for example `app/data.sqlite`), together with the
   `-wal`, `-shm`, and `-journal` files it had. Put back every database it holds:
   first remove the live `-wal`, `-shm`, and `-journal` files beside the live
   database, then move the kept database and its own sidecar files into place,
   replacing the live database. Never copy only the database file back while
   another database's sidecar files sit beside it: SQLite would apply them to the
   original database and corrupt it. A database listed in `restore.json` with
   `"existed": false` was created by the restore and has no original; remove it
   and its sidecar files if you do not want it.
2. For a **full** restore, the directory holds the original data tree as `data/`
   (and, if Tako began to put it back, the restored tree as `restored/`). Move the
   live `apps/<app>/<env>/data` aside, then move `<directory>/data` to
   `apps/<app>/<env>/data`. If `<directory>/data` is missing and `restore.json`
   says `"had_data": true`, the original tree is already live. With
   `"had_data": false` the app had no data before the restore, so there is no
   original tree: a live `data` is the restored one. Move it aside to go back to
   no data; the next start creates an empty one.

To keep the restored data instead, run the restore again after removing the
directory. Either way, then remove the leftover directory and start the app with
`tako deploy`. A normal server restart also starts it.


## Recover on a fresh server

1. Enroll the replacement server in the same network with the lost server's name. Restore access to the app's backup configuration, backup keys, and storage credentials, then deploy it.
2. List available SQLite points with `tako backups list --database <path> --server <name>`. Add `--shared` for a shared database.
3. Restore the selected point using `--latest` or `--txid`. If uploads or workflow state also need recovery, restore the relevant manual directory archive first, then restore the SQLite point.
4. Verify known records and app behavior before sending traffic to the replacement.

If remote history exists but the local spool is missing, Tako pauses capture until an explicit restore. It does not make a new empty database the latest recovery point. A fresh deploy creates no full-directory backup.

## Know the limits

Capture requires WAL-capable local SQLite and, on Linux, Landlock ABI 3. Do not replace or truncate a database file while capture is running. Application processes outside Tako's supervision must also stop before restore. The server reserves 512 MiB of free disk for capture; insufficient space pauses capture and appears in status. Remote retention does not bound a local spool during a prolonged upload outage.

Manual archives require temporary space for the snapshot, compressed archive, and encrypted archive, plus the disk reserve. They are not a transactionally consistent snapshot across multiple databases or arbitrary files being modified concurrently. Coordinate app writes when that consistency is required.

## Verify recovery

Inspect backup status and app logs, request the app, and check known records and files. Test recovery before depending on it. See [backup troubleshooting](/troubleshooting/#backups-fail) for setup and credential failures.

## Container data

Standalone container releases mount private app data at `/tako/data` by default and shared data at `/tako/shared-data`. Compose services receive the managed shared mount and `TAKO_SHARED_DATA_DIR`; named volumes remain outside Tako's backups. Select SQLite files under the shared mount with `shared_databases`. A Compose restore stops the affected stack, and a shared restore stops all stacks and native apps on that server.
