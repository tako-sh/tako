# Current secrets and administrator recovery

Follow-up to the network shared state change. Keep Corrosion on servers and
client synchronization over pinned HTTPS. Keep app configuration in Git.

## Secret storage

- Keep the deterministic winning record for each secret address, including a
  versioned tombstone for deletion. Merge independently edited addresses.
- Keep wrapped recipient keys with that record. Concurrent grants merge without
  changing the value's revision. Obsolete values and their wraps are removed
  from Corrosion, server observations, and client caches.
- Corrosion can deliver concurrent candidates separately. Replace observed
  candidates transactionally, keeping the winning payload before deleting any
  superseded candidate. Never delete an unseen concurrent write. Corrosion's
  internal replication metadata is distinct from application value history.
- Retain only bounded publication receipts and an outbox for unfinished writes.
  Expiring mutation capabilities prevent an old retry from becoming a new
  write after its receipt is collected. Expired attempts preserve local drafts
  and require explicit retry as a new operation.
- Migrate persisted ciphertext and recipient wraps; never discard the only
  usable wrap for a current value. Backup encryption keys have distinct key IDs
  and remain live records while retained backups need them.

## Host administrator recovery

- Add `sudo tako-server admin invite`, usable through provider console or
  administrator SSH without an existing client identity.
- Authenticate the host operator independently from network permissions.
  Keep recovery off the public HTTP RPC surface and serialize it through the
  running network agent rather than opening its database a second time.
- Mint the existing short-lived, single-use invitation format, including issuer
  address and certificate pin. Bind the recovery authority durably to that
  invitation; complete the normal device key-possession proof.
- A recovered administrator has independent authority, unaffected by revoking
  the lost client. Ordinary invitations retain their delegation chain.
- Recovery needs a surviving server with intact state and keys. It does not
  reconstruct data after every copy is lost.

## Verification and delivery

Cover overwrite convergence, stale snapshots and deletions, concurrent key
grants, lost responses, expired retries, persistence migration, repeated updates
beyond the previous history limit, backup key preservation, root-only recovery,
single-use claims, and recovery followed by revoking the original administrator.
Run affected Rust tests plus `just test cli` and `just test e2e`. Synchronize
docs and agent guidance using the repository commands, build and verify the
website. Commit and push incrementally to one new pull request.
