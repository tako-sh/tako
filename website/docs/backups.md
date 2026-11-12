---
title: Backups
description: "Back up Tako app data to private S3-compatible storage, inspect backup status, download encrypted archives, and restore a selected server safely."
---

Backups protect a deployed app/environment's data tree on a server. They are opt-in, compressed, encrypted, and uploaded to a private S3-compatible storage resource. Use [release rollback](/operations/#logs-and-releases) to recover code; use a backup to recover data.

Tako keeps two kinds of backup:

- A **full** archive of the whole data tree, after each successful deploy and about every 24 hours.
- A **database snapshot** of only the SQLite databases in the data tree, about every hour between full archives.

## Configure a destination

```toml
[envs.production]
backup = { storage = "private_backups" }

[storages.private_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://example.r2.cloudflarestorage.com"
region = "auto"
```

Set credentials without creating an app-facing binding:

```bash
tako storages credentials private_backups --env production
```

Deploy the configuration. Backup resources must use S3-compatible storage and must not set `public_base_url`; local storage cannot be the backup destination. A backup-only resource is not exposed through `tako.storages` unless separately bound to the app.

## Know the backup boundary

The server snapshots `/opt/tako/apps/<app>/<env>/data/`:

| Data                                                       | Included?                                                      |
| ---------------------------------------------------------- | -------------------------------------------------------------- |
| App-owned files under `data/app/`, including local SQLite  | Yes; SQLite files are snapshotted consistently.                |
| Local workflow state under `data/tako/`                    | Yes.                                                           |
| Channel replay and `tako.cache` databases                  | No; these are transient or recomputable.                       |
| Symlink targets outside the data tree                      | No; symlinks themselves are preserved.                         |
| External databases, shared Postgres, or S3 object contents | Not copied by the app-data snapshot; protect them separately.  |
| Release code and dependencies                              | Not part of the data snapshot; releases have separate history. |

A release rollback preserves the current backup target and encryption keys. Each server backs up its own local data.

## Know what is captured when

| Kind        | Created                                                                        | Contains                                                                           | Retention                                      |
| ----------- | ------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- | ---------------------------------------------- |
| `full`      | After each successful deploy, and when the latest full archive is 24 hours old | `data/app/` and `data/tako/`, with the exclusions above                            | 30 days (`retention_days` in `backups status`) |
| `databases` | About an hour after the latest backup of either kind                           | Every SQLite database under `data/app/` and `data/tako/`, including workflow state | 48 hours                                       |

A database snapshot finds databases by their SQLite file header and captures each one the same way the full archive does, then compresses and encrypts the set with the same keys. The server checks every 10 minutes whether a backup is due and skips the hourly database snapshot when:

- No database changed since the latest backup of either kind. Nothing is uploaded.
- The databases, counting each database and its `-wal` file, total more than 512 MiB. Daily full archives continue.
- Free disk space is below three times the database size plus 512 MiB.
- A deploy, another backup, or a restore of the app is running. The snapshot is retried on the next 10-minute check.

`tako backups status` reports the latest database snapshot and the reason the latest attempt was skipped.

A full archive first checks free disk: it needs three times the size of the data tree plus 512 MiB for its temporary copies, counting each file as whole 4 KiB disk blocks plus an archive header, so a tree of many small files needs far more than its byte total. When a scheduled full archive fails, for this or any other reason, it is retried after 1 hour, then 2 and 4 hours, then every 6 hours until one succeeds; hourly database snapshots keep running meanwhile. When the backup index in storage cannot be read, no scheduled backup runs for the app; the index is read again after 10 minutes, backing off to hourly. `tako backups status` shows the failure and the next attempt. The scheduler keeps this backoff in memory and waits 10 minutes after a server start before its first check.

The recovery point is therefore about an hour for SQLite data and about a day, or the latest deploy, for every other file. When hourly snapshots are skipped for size or disk space, SQLite data falls back to the daily recovery point.

Only one backup or restore can run per app at a time; a concurrent request returns
a busy error. The post-deploy backup is the exception: when a database snapshot is
running, the deploy finishes without waiting and reports the backup as queued; the
server runs it as soon as the snapshot finishes and shows a failure in
`tako backups status`. A queued backup survives a server restart and runs at the
first check after the start. Any other busy or failed post-deploy backup is reported in the
deploy output without failing the deploy. A database snapshot never waits for a
deploy; it holds the app's deployment lock only while it copies the databases
locally, and scheduled remote transfers release that lock after the local snapshot is
ready. A deploy, release command, or `tako secrets sync` that arrives while a scheduled
backup holds that lock waits up to 2 seconds, then can fail with `A scheduled backup of
<app> is in progress; retry in a moment.`; retry it, because scheduled backups then
leave the lock to the retry for about a minute and run on a later check. Failed retention deletions remain queued for a later backup attempt; expired
entries are excluded from backup listings. Remote transfers have a 30-minute total
deadline and a 60-second stalled-read deadline; metadata requests have a 30-second
deadline.

If a data file has multiple hard links, the backup fails. Replace the app's link with a private copy before retrying.

With an active SQLite write-ahead log (WAL), Tako normally captures a small log first, then lets writers commit while it copies the main database. On Linux hosts that support Landlock ABI 3 or newer, Tako uses a read-only SQLite connection restricted to the verified database files when the log exceeds 4 MiB or its lock is busy. The restricted reader can finish while a writer has uncommitted changes. On hosts without these kernel permissions, Tako uses the locked capture for WALs of any size. A checkpoint may wait until the read finishes.

In rollback-journal mode, writers can prepare changes during a snapshot. A commit or a write that needs exclusive access can wait for the copy to finish. Tako compacts the private snapshot to discard deleted records.

If Tako cannot acquire the required locks within 10 seconds, retry the backup. If a crash left a hot rollback journal, let the app reopen the database to recover it before retrying.

## Create and inspect

```bash
tako backups status --env production --json
tako backups now --env production --server la
tako backups list --env production --server la --json
```

`backups now` creates a full archive. `backups list` shows every entry, newest first, with its kind. In `--json` output each entry includes `id`, `kind` (`full` or `databases`), `created_at_unix_secs`, `size_bytes`, and the encryption metadata; database snapshots also list their `databases` as paths relative to the data directory, such as `app/data.sqlite` or `tako/workflows.sqlite`. `backups status --json` includes `last_backup` (the latest full archive), `next_backup_at_unix_secs`, `retention_days`, `last_database_snapshot_at_unix_secs`, `last_database_snapshot_skip` with `at_unix_secs`, `reason` (`unchanged`, `no_databases`, `too_large`, `insufficient_disk`, `deploy_in_progress`, or `backup_in_progress`), and `message`, and `last_backup_failure` with `at_unix_secs`, `reason` (`failed`, `insufficient_disk`, or `index_unreadable`), `message`, and `retry_at_unix_secs` while full archives are failing or the index cannot be read.

Use explicit environment and server names when selecting recovery data. Download an archive by the ID returned by the list:

```bash
tako backups download <backup-id> --env production --server la
```

Without `--output`, a full archive is saved as `<backup-id>.tar.zst.enc` and a database snapshot as `<backup-id>.databases.tar.zst.enc` in the current directory. The name is built from the backup ID, never from the storage object key, and the download refuses an index entry whose ID is not one Tako creates. Both are encrypted with the environment's backup key; once decrypted, each is a zstd-compressed tar archive:

| Kind        | Archive contents                                                                                                                                                                                   |
| ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `full`      | `app/` and `tako/`, the data tree as captured.                                                                                                                                                     |
| `databases` | `databases/app/` and `databases/tako/`, holding only the databases at their paths in the data tree. Each is a compacted copy in rollback-journal mode, with no `-wal`, `-shm`, or `-journal` file. |

Retain access to the environment-managed backup keys as well as the storage credentials. Download and restore require `--server` when the environment has multiple servers.

## Restore data

Confirm the app, environment, server, and backup ID before restoring:

```bash
tako backups restore <backup-id> --env production --server la
```

The confirmation says what the selected backup replaces. Restore verifies the archive checksum and decrypts it, stops app and workflow processes, replaces data, then synchronizes workflows and resumes the app according to its desired instance state. It replaces data and can interrupt service; it is not a merge or a code rollback. An agent must establish authorization for the selected restore before using a confirmation-skipping flag.

Restoring a **full** archive replaces the whole data tree.

Restoring a **database snapshot** replaces only the SQLite databases it contains, inside the existing data tree:

- Each database is replaced atomically. Its `-wal`, `-shm`, and `-journal` files are removed so they cannot bring newer data back.
- Other files, and databases that are not in the snapshot, stay untouched. The summary and `--json` output (`databases.replaced`, `databases.created`, `databases.untouched`) list them.
- A database whose path no longer exists is created, along with missing directories.
- A file named like a sidecar, such as `audit-journal`, is a sidecar only when the database it is named after (`audit`) is beside it; otherwise it is a database like any other. Restore refuses a database whose name would pair it with another live database.
- Restored databases get the modes app provisioning sets (`0660` under `data/app/`, `0600` under `data/tako/`), not the modes of the files they replace.
- Restore refuses, before changing anything, a path that is now a symlink, passes through a symlink or a file, is a hard link, or holds a file that is not a SQLite database.
- Restoring `tako/workflows.sqlite` rewinds workflow runs and their saved steps to the snapshot time. Runs enqueued later are gone, and runs that finished later are pending again and can run again.
- The server needs free space for the downloaded object twice over, then for the extracted databases, plus 512 MiB, because the originals stay on disk until the app restarts.

Tako blocks cold starts and health replacements while swapping data. Before it
touches any file it stops every workflow worker of the app, including workers of a
replaced release that are still finishing runs (they get 2 minutes, then are
killed), and closes the server's own handles on the workflow store; workflow run
commands are refused until the restore finishes. Database files are inspected and
swapped by a separate worker process; a worker that cannot start changes nothing,
and one still running after 15 minutes is stopped and its swap undone. Restore
retains the previous data beside the data directory, in
`apps/<app>/<env>/.data-restore-prev-<id>/`, until restart succeeds. If the swap or
restart fails, Tako stops the restored processes, puts the original data back,
including SQLite sidecar files, and restarts the app on it. If the original data
could not be put back, the app is left stopped and the error names the directory
that holds it. If the restart on the original data also fails, the app is left
stopped for inspection.

Each restore first writes a record of what it will change. If the server process
dies in the middle of a restore, the next normal server start puts the original
data back before any app starts, logs it, and removes leftover backup and restore
work directories. The recovery point of that restore is not applied; run it again.
A standby, an upgrade reload, a promotion, and a normal start while another server
process (such as a hot standby) serves the same data directory never put data
back; they only log an unfinished restore. A normal start renames every
unfinished restore it does not put back to `.data-restore-abandoned-<id>/`, so no
later start puts it back over data written since.

When the server gives up on a restore and keeps running (it could not stop the
restored app, could not put the original data back, or could not record the
restore as finished), it renames the directory to
`.data-restore-abandoned-<id>/` and logs an error. Tako never applies such a
directory: whatever is live stays live. While an unfinished or abandoned restore
directory exists, a new restore of the app refuses and names it. Server starts
log every such directory until it is removed. If the live data may be partly
restored, the app also stays stopped; see
[Resolve an unfinished restore](#resolve-an-unfinished-restore).

Restore moves files with rename, so the app data directory must be on the same
filesystem and mount as the Tako data directory. When `data/` or a directory a
restored database sits in is a separate mount (including a bind mount), restore
refuses before changing anything. A full restore also refuses while anything is
mounted anywhere below `data/`, such as `data/app/uploads`, because it replaces the
whole directory. Removing a rollback copy never crosses into another mount.

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
  stopped: server starts do not start it or its workflows, requests do not cold
  start it, `tako status` shows the reason as its error, and deploys, release
  commands, `tako scale`, and secret updates are refused with the same message.

A directory without `restore.json` means the live data is whole, either the
original or the restored data; remove it.

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
   `apps/<app>/<env>/data`. If `<directory>/data` is missing, the original tree
   is already live.

To keep the restored data instead, run the restore again after removing the
directory. Either way, then remove the leftover directory and start the app with
`tako deploy`. A normal server restart also starts it.

## Recover on a fresh server

Each server lists and restores the backups recorded under its own server name. To recover a lost server:

1. Add the replacement server with the lost server's name, then deploy the app with the same backup configuration and keys.
2. Run `tako backups list --env production --server <name> --json` and note the entries created before the loss. The fresh deploy also creates new backups of the empty data tree.
3. Restore the newest full archive from before the loss.
4. If a database snapshot from before the loss is newer than that archive, restore the newest one. It brings SQLite data, including workflow state, forward to within about an hour of the loss.

Files other than SQLite databases come back as of the full archive.

## Know the limits

Backups do not cover Docker named volumes, external databases, shared Postgres state, S3 object contents, channel replay, or `tako.cache` entries. Hourly database snapshots stop while the databases total more than 512 MiB, and each server backs up only its own data. Restore a database snapshot onto the data tree it was taken from, or onto a restored full archive; it does not bring back files that are not SQLite databases.

## Verify recovery

Inspect status and finite app logs, request the deployed app, and check a known record or file from the restored snapshot. Verify a recovery procedure before depending on backups for an application. If setup or credential checks fail, use [backup troubleshooting](/troubleshooting/#backups-fail).

## Container data

Standalone container releases mount the app data directory, so its files follow the same backup and restore path as native releases. Compose named volumes are managed by Docker and are outside the Tako backup. Back them up with database-native dumps or a coordinated volume backup, and test restoration separately from application rollback.
