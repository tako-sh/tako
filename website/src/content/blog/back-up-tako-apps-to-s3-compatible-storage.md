---
title: "Back Up Tako Apps to S3-Compatible Storage"
date: "2026-05-24T14:34"
description: "Configure Tako backups with private S3-compatible storage, encrypted archives, manual restores, and deploy-time snapshots for app data."
image: f7c38670efaf
---

The cute version of persistent app storage is "SQLite and uploads on a $5 VPS."

The grown-up version is "and I can restore it when the disk disappears."

Tako already gives every app a persistent data directory for SQLite files, uploads, workflow state, channel replay logs, and other file-backed state. We covered that in [Stateful Apps on Tako](/blog/stateful-apps-sqlite-uploads-tako-data-dir): deploys swap releases, but the data directory stays put.

Backups are the other half of that story. Add one private S3-compatible storage resource to `tako.toml`, give Tako encrypted credentials, and `tako-server` can archive app data after deploys, on a regular cadence, and on demand.

No cron script. No rsync folder on the same machine. No "I thought the VPS provider snapshots were enabled."

## What gets backed up

When backups are enabled, Tako backs up the whole per-app data tree for that app and environment. That includes app-owned files and Tako-owned runtime state:

| Path in the backup | What it contains                                                    |
| ------------------ | ------------------------------------------------------------------- |
| `app/`             | Your app data exposed through `TAKO_DATA_DIR` and SDK helpers       |
| `tako/`            | Tako-owned per-app state such as channels and workflow SQLite files |

That matters because a modern app is rarely just one database file. A small project may have:

| Data               | Common location         |
| ------------------ | ----------------------- |
| SQLite database    | `tako.dataDir/app.db`   |
| Uploaded avatars   | `tako.dataDir/uploads/` |
| Queue or job state | Tako workflow storage   |
| Realtime replay    | Tako channel storage    |

The archive format is designed for app data, not just raw file copying. SQLite files are snapshotted with SQLite's online `VACUUM INTO` mechanism before archiving, so Tako does not need to include `-wal` and `-shm` companion files separately. The result is compressed as `tar.zst`, encrypted before upload with AES-256-GCM, and tracked with a SHA-256 manifest plus a remote JSON index.

The short version: Tako backs up the state your app needs to come back, and the object store receives encrypted archives.

The [deployment docs](/docs/deployment) show where backups fit in the deploy flow, and the [`tako.toml` reference](/docs/tako-toml) has the exact config shape.

## Configure a private S3-compatible resource

Backups are opt-in per environment:

```toml
[envs.production]
route = "app.example.com"
servers = ["prod-a"]
backup = { storage = "r2_backups" }

[storages.r2_backups]
provider = "s3"
bucket = "app-backups"
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
```

The storage resource is normal Tako storage metadata: bucket, endpoint, region, and provider. `provider = "s3"` means an S3-compatible API, so the endpoint can point at an S3-compatible service such as Cloudflare R2. R2 documents its storage access through an [S3-compatible API](https://developers.cloudflare.com/r2/api/s3/api/), which is why the example uses `region = "auto"` and an R2 endpoint.

The backup target must be private:

| Rule                            | Why                                                     |
| ------------------------------- | ------------------------------------------------------- |
| It must use `provider = "s3"`   | Backups upload to S3-compatible object storage          |
| It cannot use `local`           | A backup on the same local disk is not much of a backup |
| It cannot set `public_base_url` | Backup objects are not public app assets                |

If you already use S3-compatible storage for public uploads, you can still share the same bucket. Keep the backup resource private. If your upload resource has `public_base_url`, declare a second private resource that points at the same bucket and use that for `backup = { storage = "..." }`. Tako writes backup objects under its reserved `_tako/backups/{app}/{env}/{server}/` prefix, so normal app objects and backup archives do not collide.

Then add credentials:

```bash
tako storages credentials r2_backups --env production
```

That command sets or rotates encrypted S3 credentials for a declared top-level storage resource without exposing it to app code. Backup storage does not become `tako.storages.r2_backups`; it is server infrastructure, not an app binding.

If you want one resource to be both an app storage binding and a backup target, add it with the normal storage command and keep it private:

```bash
tako storages add uploads \
  --env production \
  --resource r2_private \
  --provider s3 \
  --bucket app-data \
  --endpoint https://<account>.r2.cloudflarestorage.com \
  --region auto
```

The [CLI reference](/docs/cli) covers both `tako storages add` and `tako storages credentials`.

## What happens during deploy

Backups join the deploy path early enough to catch configuration mistakes before you wait on a build.

During `tako deploy`, the CLI validates the selected environment, server mappings, app secrets, storage credentials, and backup storage credentials before build and upload work starts. Expired storage credentials fail the deploy. Credentials expiring within 30 days produce a warning.

When backups are enabled, `tako deploy` and `tako backups now` also create backup encryption keys for the environment if none exist. Those backup keys live in `.tako/secrets.json`, encrypted with the same environment key model used by Tako secrets. The latest backup key is used for new archives, and each backup manifest records the key id it needs so older backups can still be restored while that key remains available.

```d2
direction: right

deploy: "tako deploy" {
  style.fill: "#FFF9F4"
}

validate: "validate backup config\nand credentials" {
  style.fill: "#9BC4B6"
}

roll: "rolling update" {
  style.fill: "#FFF9F4"
}

snapshot: "snapshot app data" {
  style.fill: "#E88783"
}

archive: "tar.zst + AES-256-GCM" {
  style.fill: "#9BC4B6"
}

s3: "private S3-compatible bucket" {
  shape: cylinder
  style.fill: "#FFF9F4"
}

deploy -> validate -> roll -> snapshot -> archive -> s3
```

After a successful rolling update, the server creates a post-deploy app data backup. If that post-deploy backup fails, the failure is reported in the finalize response and logs, but Tako does not roll back an otherwise successful deploy. Your new release can still be serving traffic while you investigate the backup warning.

After that, `tako-server` runs due backups roughly every 24 hours while it is running. Retention defaults to 30 days.

Manual commands use the same signed HTTP management path as other app operations:

```bash
tako backups status --env production
tako backups now --env production
tako backups list --env production
```

`status` shows whether backups are enabled per mapped server, plus last and next backup timing. `now` creates an immediate backup. `list` reads the remote index and shows backup ids newest first.

## Restore is the feature

A backup system is only useful if restore is boring.

Download an encrypted archive when you want a local copy:

```bash
tako backups download b123 \
  --env production \
  --server prod-a \
  --output ./backup.tar.zst.enc
```

Restore when you need the server back in place:

```bash
tako backups restore b123 \
  --env production \
  --server prod-a \
  --yes
```

If an environment runs on multiple servers, pass `--server`. Each server has its own app data tree, and Tako includes the server name in the backup prefix so one server's archives do not overwrite another's.

During restore, Tako stops the selected server's app, replaces its data tree with the archive contents, reconciles workflows, and restarts according to the app's desired instance count. That is intentionally direct. Restoring production data should feel like a command you can understand while your pulse is not especially calm.

The operational habit is simple:

| Task                   | Command or config                                              |
| ---------------------- | -------------------------------------------------------------- |
| Enable backups         | `backup = { storage = "r2_backups" }`                          |
| Set backup credentials | `tako storages credentials r2_backups --env production`        |
| Verify state           | `tako backups status --env production`                         |
| Take one now           | `tako backups now --env production`                            |
| Restore one            | `tako backups restore <id> --env production --server <server>` |

## Keep the small-server story honest

Tako makes small servers useful: persistent app data, local SQLite, uploads, workflows, channels, routing, TLS, deploys, and scale-to-zero all live close to your app. But "small" should not mean "fragile."

Backups to S3-compatible storage keep the one-machine workflow practical without pretending the machine is immortal. Your app keeps using local files. Tako handles the archive, encryption, object key prefix, deploy hook, daily cadence, and restore command.

Start with the [`tako.toml` backup reference](/docs/tako-toml), keep the [CLI backup commands](/docs/cli) nearby, and read [what happens during `tako deploy`](/blog/what-happens-when-you-run-tako-deploy) if you want the full deploy pipeline around it.
