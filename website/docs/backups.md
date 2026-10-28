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

Deploy creates a backup after successful finalization when enabled. The server scheduler also creates due backups at roughly 24-hour intervals. Backups are per server; one server's local data does not automatically represent another's.

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

## Verify recovery

Inspect status and finite app logs, request the deployed app, and check a known record or file from the restored snapshot. Verify a recovery procedure before depending on backups for an application. If setup or credential checks fail, use [backup troubleshooting](/troubleshooting/#backups-fail).
