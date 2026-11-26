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

Backups are encrypted with the database's own key, which the network stores for full clients. Any moment of the last 7 days can be restored. Full snapshots, captured daily while the database changes and on demand with `tako db backup`, are kept longer: all from the last 24 hours, then one per day for 7 days and one per week for 4 weeks. See [Databases](/databases/#back-up-and-restore) for the full behavior, import and export, and replica replacement.

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



## Files in the data directories

`TAKO_DATA_DIR` is private to an app/environment. `TAKO_SHARED_DATA_DIR` is shared by apps on one server, including different environments on that server. Both are local disk, not network filesystems. Tako does not back up, discover, or capture anything in them: files there, including SQLite files and [local storage](/storage/) objects, are lost with the server. Copy them yourself if they must survive, or keep them in a hosted database or an S3 storage.

Apps retain separate Unix users. Tako grants shared-directory access through their common group; apps must create shared files and directories writable by that group. Never put app-private secrets in the shared directory.

## Replace a failed replica

When a voting majority survives, add the replacement server to the environment's deploy targets and deploy. Preparing waits for each selected database and KV store to catch up before that app starts. Use `tako db restore` or `tako kv restore` to roll back the whole resource to an uploaded recovery point. Restoring to the group requires a working quorum.

## Recover SQL to a local file

On a full-access workstation that has synchronized the network, recover an uploaded point directly from backup storage. The database servers can all be offline.

```bash
tako db restore orders --at 15m --output ./orders-recovered.db
```

Tako uses the cached generation and recovery key, the project’s configured backup storage, and the storage credentials for `production`. Select another environment with `--env staging`. Use `--latest` or `--snapshot <id>` in place of `--at`. To recover a retired generation, pass its ID with `--generation <id>` and configure the storage that holds its backups.

The command writes a verified, portable database to a new file. An existing destination is preserved. The hosted database keeps serving its current state.

## Know the limits

Each database is backed up on its own: backups of several databases are not a transactionally consistent snapshot across them. Coordinate app writes when that consistency is required.

A database restore needs free space for about three times what it downloads plus 256 MiB on the server's data disk, counting the space other restores and transfers in progress on that server still hold. Until storage accepts them, captures wait on the server's disk. The server uploads for up to eight databases at once, giving each upload time for its size, so one whose storage is slow does not hold up the others. A failed upload is retried with backoff, at most 15 minutes apart, and reported by `tako db status`.

## Verify recovery

Inspect `tako db status --json`, `tako kv status --json`, and app logs, request the app, and check known records. Test recovery before depending on it. See [backup troubleshooting](/troubleshooting/#backups-fail) for setup and credential failures.

## Container data

Standalone container releases mount private app data at `/tako/data` by default and shared data at `/tako/shared-data`. Compose services receive the managed shared mount and `TAKO_SHARED_DATA_DIR`. Neither these mounts nor named volumes are backed up by Tako. Container and Compose releases cannot bind hosted databases yet.
