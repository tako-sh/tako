---
title: Backups
description: "Back up Tako app data to private S3-compatible storage, inspect backup status, download encrypted archives, and restore a selected server safely."
---

Backups protect a deployed app/environment's data tree on a server. They are opt-in, compressed, encrypted, and uploaded to a private S3-compatible storage resource. Use [release rollback](/operations/#logs-and-releases) to recover code; use a backup to recover data.

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

Deploy creates a backup after successful finalization when enabled. The server scheduler also creates due backups at roughly 24-hour intervals. A release rollback preserves the current backup target and encryption keys. Each server backs up its own local data.

Only one backup or restore can run per app at a time; a concurrent request returns
a busy error. Scheduled remote transfers release the app's deployment lock after
the local snapshot is ready. Failed retention deletions remain queued for a later
backup attempt; expired entries are excluded from backup listings. Remote
transfers have a 30-minute total deadline and a 60-second stalled-read deadline;
metadata requests have a 30-second deadline.

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

Use explicit environment and server names when selecting recovery data. Download an archive by the ID returned by the list:

```bash
tako backups download <backup-id> --env production --server la \
  --output ./backup.tar.zst.enc
```

Downloads are encrypted archives. Retain access to the environment-managed backup keys as well as the storage credentials. Download and restore require `--server` when the environment has multiple servers.

## Restore data

Confirm the app, environment, server, and backup ID before restoring:

```bash
tako backups restore <backup-id> --env production --server la
```

Restore verifies the archive checksum and decrypts it, stops app and workflow processes, replaces the data tree, then synchronizes workflows and resumes the app according to its desired instance state. It replaces data and can interrupt service; it is not a merge or a code rollback. An agent must establish authorization for the selected restore before using a confirmation-skipping flag.

Tako blocks cold starts and health replacements while swapping data. It retains
the previous data until restart succeeds. If restart fails, Tako stops the
restored processes, puts the original data back, and leaves the app stopped for
inspection. If cleanup or recovery itself fails, the error identifies retained
data for manual recovery.

## Verify recovery

Inspect status and finite app logs, request the deployed app, and check a known record or file from the restored snapshot. Verify a recovery procedure before depending on backups for an application. If setup or credential checks fail, use [backup troubleshooting](/troubleshooting/#backups-fail).

## Container data

Standalone container releases mount the app data directory, so its files follow the same backup and restore path as native releases. Compose named volumes are managed by Docker and are outside the Tako backup. Back them up with database-native dumps or a coordinated volume backup, and test restoration separately from application rollback.
