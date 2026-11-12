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

The recovery point is therefore about an hour for SQLite data and about a day, or the latest deploy, for every other file. When hourly snapshots are skipped for size or disk space, SQLite data falls back to the daily recovery point.

Only one backup or restore can run per app at a time; a concurrent request returns
a busy error. The post-deploy backup is the exception: it waits up to 5 minutes for
a running database snapshot to finish, and otherwise reports the busy error in the
deploy result without failing the deploy. A database snapshot never waits for a
deploy; it holds the app's deployment lock only while it copies the databases
locally, and scheduled remote transfers release that lock after the local snapshot is
ready. Failed retention deletions remain queued for a later backup attempt; expired
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

`backups now` creates a full archive. `backups list` shows every entry, newest first, with its kind. In `--json` output each entry includes `id`, `kind` (`full` or `databases`), `created_at_unix_secs`, `size_bytes`, and the encryption metadata; database snapshots also list their `databases` as paths relative to the data directory, such as `app/data.sqlite` or `tako/workflows.sqlite`. `backups status --json` includes `last_backup` (the latest full archive), `next_backup_at_unix_secs`, `retention_days`, `last_database_snapshot_at_unix_secs`, and `last_database_snapshot_skip` with `at_unix_secs`, `reason` (`unchanged`, `no_databases`, `too_large`, `insufficient_disk`, `deploy_in_progress`, or `backup_in_progress`), and `message`.

Use explicit environment and server names when selecting recovery data. Download an archive by the ID returned by the list:

```bash
tako backups download <backup-id> --env production --server la
```

Without `--output`, a full archive is saved as `<backup-id>.tar.zst.enc` and a database snapshot as `<backup-id>.databases.tar.zst.enc` in the current directory. Both are encrypted with the environment's backup key; once decrypted, each is a zstd-compressed tar archive:

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
- Restore refuses, before changing anything, a path that is now a symlink, passes through a symlink or a file, is a hard link, or holds a file that is not a SQLite database.
- Restoring `tako/workflows.sqlite` rewinds workflow runs and their saved steps to the snapshot time. Runs enqueued later are gone, and runs that finished later are pending again and can run again.
- The server needs free space for the downloaded object twice over, then for the extracted databases, plus 512 MiB, because the originals stay on disk until the app restarts.

Tako blocks cold starts and health replacements while swapping data. It retains
the previous data until restart succeeds. If the swap or restart fails, Tako stops
the restored processes, puts the original data back, including SQLite sidecar
files, and restarts the app on it. If that restart also fails, the app is left
stopped for inspection. If cleanup or recovery itself fails, the error identifies
retained data for manual recovery.

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
