---
title: "Backups"
description: "Back up Tako-hosted databases continuously with point-in-time restore, and create or restore encrypted manual archives of an app's data directory."
---

Tako protects two kinds of data separately:

| Data                                                                         | Protection                                                                                                                                   | Configure                                             |
| ---------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| [Hosted databases](/databases/)                                              | Continuous encrypted backups: changes every 10 seconds, restorable to any moment of the last 7 days, with daily and on-demand full snapshots | `[envs.<env>].backup.storage` or `[db.<name>].backup` |
| [KV stores](/kv/)                                                            | Encrypted snapshots: hourly while changing, before release commands and restores, and on demand                                              | `[envs.<env>].backup.storage` or `[kv.<name>].backup` |
| Files under the app's `data/app/` and `data/tako/`, including workflow state | Manual encrypted full-directory archives                                                                                                     | `[envs.<env>].backup`                                 |
| Files in the shared data directory                                           | Not backed up; copy them separately during coordinated downtime                                                                              | —                                                     |
| Compose named volumes, external databases, S3 contents                       | Protect separately                                                                                                                           | —                                                     |
| Network membership, shared secrets, device keys                              | Protect separately                                                                                                                           | —                                                     |

Use [release rollback](/operations/#logs-and-releases) to recover code and a backup to recover data. Deploys and timers do not create app-directory archives.

## Back up hosted databases

Set the environment's `backup.storage` (or `[db.<name>].backup` for one database) to a private S3-compatible storage resource and deploy. The database's server then backs it up continuously: every 10 seconds it uploads the transactions committed since the last upload, and it records a restore point before each deploy's release command, before migrations, and before a restore, import, or reset:

```toml
[envs.production]
db = { main = "main" }
backup = { storage = "private_backups" }
```

```bash
tako db backups main --json
tako db restore main --at 15m
tako db restore main --snapshot <id>
tako db backup main
```

Backups are encrypted with the database's own key, which the network stores for full clients. Any moment of the last 7 days can be restored. Full snapshots, captured daily while the database changes and on demand with `tako db backup`, are kept longer: all from the last 24 hours, then one per day for 7 days and one per week for 4 weeks. See [Databases](/databases/#back-up-and-restore) for the full behavior, import and export, and recovery onto another server.

## Configure an app data destination

```toml
[envs.production]
backup = { storage = "private_backups" }

[storages.private_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://s3.example.com"
region = "auto"
```

```bash
tako storage credentials private_backups --env production
tako deploy --env production
```

The destination must be private S3-compatible storage without `public_base_url`. A backup-only storage resource is not exposed through `tako.storages` unless separately bound to the app. Keep the environment-managed backup keys and storage credentials: the server cannot recover encrypted archives without them. One storage resource can receive both app archives and database backups.

### Upgrade from SQLite file capture

Earlier versions listed SQLite files in `backup = { databases = [...], shared_databases = [...] }` and captured them continuously. Those fields are rejected now. Move each database to a hosted database:

1. `tako db create <name> --server <server>`, then bind it with `db = { <binding> = "<name>" }` under `[envs.production]` in `tako.toml`. Its backups go to the environment's `backup.storage`, or to `[db.<name>].backup`.
2. During a write freeze, copy the existing file to your machine, checkpoint it (`PRAGMA wal_checkpoint(TRUNCATE)`), and run `tako db import <name> <file>`.
3. Change the app to use `tako.db.<binding>` and deploy.

Capture history uploaded by earlier versions stays in its bucket, but this version cannot restore it. A SQLite file left in the data directory is still included in manual archives.

## Know the archive boundary

`TAKO_DATA_DIR` is private to an app/environment. `TAKO_SHARED_DATA_DIR` is shared by apps on one server, including different environments on that server. Both are local disk, not network filesystems, and Tako does not discover or capture databases in them.

Apps retain separate Unix users. Tako grants shared-directory access through their common group; apps must create shared files and directories writable by that group. Never put app-private secrets in the shared directory.

Manual archives include every file under the app's `data/app/` and `data/tako/`, including Tako workflow state. SQLite files are copied through a consistent SQLite snapshot. Archives exclude transient channel and cache databases and retain symlinks as symlinks; they do not follow a symlink to include shared data.

## Create and inspect

```bash
tako backup now --env production --server la
tako backup list --env production --server la --json
tako backup status --env production --json
```

Archive status reports the last manual full archive, the last failed attempt, retention, and unfinished restores. Full archives retain 30 days by default; their cleanup runs when an archive is created, not on a timer. `backup list` also shows `databases` archives created by older servers.

Creating, listing, downloading and restoring backups needs a full client. A deploy client can change an environment's backup settings in `tako.toml`, which apply when it deploys, but cannot run these commands.

Download a manual archive with `tako backup download <backup-id> --env production --server la`. The result is an encrypted zstd-compressed tar archive.

## Restore app data

```bash
tako backup restore <backup-id> --env production --server la
```

Choose the server explicitly when the environment has multiple servers. A restore downloads and verifies the archive, stops the app and its workflow workers, replaces `data/app/` and `data/tako/` together, and restarts the app. This can rewind workflow state. If restart fails, Tako attempts to stop the failed runtime and put the original files back. Archives of type `databases` from older Tako versions remain restorable.

A standby process using the same data directory must be stopped before restoring. Tako refuses the restore while it holds the standby lease.

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
  updates, and `tako backup now` are refused with the same message. A manual backup is refused.

A directory without `restore.json` means the live data is whole, either the
original or the restored data; remove it.

Shared database restores from earlier Tako versions left these directories below
`shared/`, beside `shared/data/`. The same states and database recovery steps
apply, with every app kept stopped while repairing shared data. Once the files
and restore record are resolved, use `tako network server resume <server>` to
clear the persisted drain and restart the apps.

To go back to the original data after a partial restore, with the app stopped,
work as the user that owns the app data:

1. For a restore of an older **database snapshot** archive, the directory holds each original database
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

1. Enroll the replacement server in the network. Restore access to the app's backup configuration, backup keys, and storage credentials.
2. Recover each hosted database with `tako db restore <name> --latest --server <new-server>`, and each KV store with `tako kv restore <name> --latest --server <new-server>`. Each keeps its name and points its network record at the new server. A KV store without backups cannot be recovered: once the lost server is removed from the network, `tako kv delete <name>` forgets it, and `tako kv create <name> --server <new-server>` creates an empty one under the same name.
3. Deploy the app to the replacement server.
4. If uploads or workflow state also need recovery, restore the relevant manual archive with `tako backup restore <backup-id> --server <new-server>`.
5. Verify known records and app behavior before sending traffic to the replacement.

A fresh deploy creates no full-directory backup.

## Know the limits

Manual archives require temporary space for the snapshot, compressed archive, and encrypted archive, plus a disk reserve. They are not a transactionally consistent snapshot across multiple databases or arbitrary files being modified concurrently. Coordinate app writes when that consistency is required. Application processes outside Tako's supervision must stop before a restore.

A database restore needs free space for about three times what it downloads plus 256 MiB on the server's data disk, counting the space other restores and transfers in progress on that server still hold. Until storage accepts them, captures wait on the server's disk. The server uploads for up to eight databases at once, giving each upload time for its size, so one whose storage is slow does not hold up the others. A failed upload is retried with backoff, at most 15 minutes apart, and reported by `tako db status`.

## Verify recovery

Inspect `tako db status --json`, `tako backup status --json`, and app logs, request the app, and check known records and files. Test recovery before depending on it. See [backup troubleshooting](/troubleshooting/#backups-fail) for setup and credential failures.

## Container data

Standalone container releases mount private app data at `/tako/data` by default and shared data at `/tako/shared-data`. Compose services receive the managed shared mount and `TAKO_SHARED_DATA_DIR`; named volumes remain outside Tako's backups. Container and Compose releases cannot bind hosted databases yet. A Compose restore stops the affected stack.
