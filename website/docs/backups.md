---
title: "Backups"
description: "Back up Tako-hosted databases continuously with point-in-time restore, and KV stores with encrypted snapshots."
---

Tako backs up the data it hosts. Everything else is yours to protect:

| Data                                                   | Protection                                                                                                                                   | Configure                                             |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| [Hosted databases](/databases/)                        | Continuous encrypted backups: changes every 10 seconds, restorable to any moment of the last 7 days, with daily and on-demand full snapshots | `[envs.<env>].backup.storage` or `[db.<name>].backup` |
| [KV stores](/kv/)                                      | Encrypted snapshots: hourly while changing, before release commands and restores, and on demand                                              | `[envs.<env>].backup.storage` or `[kv.<name>].backup` |
| Files in `TAKO_DATA_DIR` and `TAKO_SHARED_DATA_DIR`    | Not backed up by Tako; copy them yourself, or keep objects that must survive in an S3 [storage](/storage/)                                   | —                                                     |
| Compose named volumes, external databases, S3 contents | Protect separately                                                                                                                           | —                                                     |
| Network membership, shared secrets, device keys        | Protect separately                                                                                                                           | —                                                     |

Use [release rollback](/operations/#logs-and-releases) to recover code and a backup to recover data.

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

## Configure the destination

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

The environment's `backup.storage` is the default destination of every hosted database and KV store it binds; `[db.<name>].backup` and `[kv.<name>].backup` override it for one. The destination must be private S3-compatible storage without `public_base_url`. A backup-only storage resource is not exposed through `tako.storages` unless separately bound to the app. Keep the storage credentials: the server cannot reach its backups without them.

### Upgrade from SQLite file capture

Earlier versions listed SQLite files in `backup = { databases = [...], shared_databases = [...] }` and captured them continuously. Those fields are rejected now. Move each database to a hosted database:

1. `tako db create <name> --server <server>`, then bind it with `db = { <binding> = "<name>" }` under `[envs.production]` in `tako.toml`. Its backups go to the environment's `backup.storage`, or to `[db.<name>].backup`.
2. During a write freeze, copy the existing file to your machine, checkpoint it (`PRAGMA wal_checkpoint(TRUNCATE)`), and run `tako db import <name> <file>`.
3. Change the app to use `tako.db.<binding>` and deploy.

Capture history uploaded by earlier versions stays in its bucket, but this version cannot restore it.

## Files in the data directories

`TAKO_DATA_DIR` is private to an app/environment. `TAKO_SHARED_DATA_DIR` is shared by apps on one server, including different environments on that server. Both are local disk, not network filesystems. Tako does not back up, discover, or capture anything in them: files there, including SQLite files and [local storage](/storage/) objects, are lost with the server. Copy them yourself if they must survive, or keep them in a hosted database or an S3 storage.

Apps retain separate Unix users. Tako grants shared-directory access through their common group; apps must create shared files and directories writable by that group. Never put app-private secrets in the shared directory.

## Recover on a fresh server

1. Enroll the replacement server in the network. Restore access to the app's backup configuration and storage credentials.
2. Recover each hosted database with `tako db restore <name> --latest --server <new-server>`, and each KV store with `tako kv restore <name> --latest --server <new-server>`. Each keeps its name and points its network record at the new server. A KV store without backups cannot be recovered: once the lost server is removed from the network, `tako kv delete <name>` forgets it, and `tako kv create <name> --server <new-server>` creates an empty one under the same name.
3. Deploy the app to the replacement server.
4. Verify known records and app behavior before sending traffic to the replacement.

## Know the limits

Each database is backed up on its own: backups of several databases are not a transactionally consistent snapshot across them. Coordinate app writes when that consistency is required.

A database restore needs free space for about three times what it downloads plus 256 MiB on the server's data disk, counting the space other restores and transfers in progress on that server still hold. Until storage accepts them, captures wait on the server's disk. The server uploads for up to eight databases at once, giving each upload time for its size, so one whose storage is slow does not hold up the others. A failed upload is retried with backoff, at most 15 minutes apart, and reported by `tako db status`.

## Verify recovery

Inspect `tako db status --json`, `tako kv status --json`, and app logs, request the app, and check known records. Test recovery before depending on it. See [backup troubleshooting](/troubleshooting/#backups-fail) for setup and credential failures.

## Container data

Standalone container releases mount private app data at `/tako/data` by default and shared data at `/tako/shared-data`. Compose services receive the managed shared mount and `TAKO_SHARED_DATA_DIR`. Neither these mounts nor named volumes are backed up by Tako. Container and Compose releases cannot bind hosted databases yet.
