# Continuous SQLite backups

Run from the repository root:

```sh
just test terse
cargo clippy --locked -p tako-backup --all-targets -- -D warnings
```

This crate backs up hosted databases continuously. `Capture` turns committed
WAL pages into LTX files in a local spool, using the vendored
`terse-litestream` port of Litestream (see `vendor/README.md`). `Replica`
uploads them to object storage, encrypted with the database's key, restores
any captured transaction, and applies retention.

## Layout

Under a database's prefix (`_tako/databases/<name>/<generation>/`):

- `ltx/<level>/<min>-<max>-<captured>.ltx`: encrypted segments, named with
  the capture time from their header (Unix ms). Level 9 holds full images
  (bases) from transaction 1; level 0 holds one capture each.
- `points/<unix-ms>-<txid>-<reason>`: empty markers naming a transaction,
  such as the one before a deploy or a manual snapshot.

Segments are created only where none exists. An existing object is
downloaded and compared before it is accepted, so a lost acknowledgement is
retried safely and a conflicting writer is detected. Segments over 1 GiB go
up in 64 MiB parts, since single requests are limited to 5 GiB. An object is
one AES-GCM message, so a segment over 64 GiB is refused. A multipart
write cannot be conditional, so for those an existing object is looked for
first; two owners never write one key, because a new owner starts above the
history and a fenced one stops uploading.

## Behavior

- Capture and upload run every **10 seconds** for a changed database; idle
  databases make no remote requests. Failed uploads back off up to 15
  minutes while capture continues locally.
- Uploaded segments leave the spool, except the newest, which holds the
  capture position.
- Opening capture writes a full image. A changing database gets another every
  24 hours, so a restore downloads one base and at most about a day of
  captures.
- A capture with an empty spool starts 2^32 transactions above the existing
  history. A previous owner that keeps uploading for a while (a server the
  database moved away from) never shares a transaction ID with it, so no
  restore chain mixes the two.
- A restore targets the latest transaction, a transaction ID, or a time: the
  newest capture at or before it, found from the capture times in the
  listing without downloading anything. It downloads only the segments it
  needs, verifies their checksums, and never replaces an existing file.
- Retention keeps every transaction of the last **seven days**: from the newest
  base captured before then, everything stays. Older bases the caller keeps
  (long-term full snapshots) stay with their points; the rest is deleted,
  markers first.
