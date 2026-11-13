# Network shared state implementation plan

Keep application source and desired deployment configuration in `tako.toml` in
Git. Make network membership and published encrypted secrets available through
servers, with authenticated client caches and isolated worktree drafts. Deliver
the refactor in one pull request with incremental, tested commits.

## Ownership and command contract

- Corrosion remains the server replication transport. Clients talk only to
  servers over pinned, signed HTTPS. Clients do not run Corrosion or communicate
  with other clients.
- Membership is the only server inventory. Remove `ServersToml`, its parser and
  persistence, and legacy inventory fallback. Retain an in-memory resolved
  inventory for command target selection. Local SSH paths and installer access
  remain local device settings.
- A network has a permanent ID and a shared human name. Names are used in human
  output; machine-readable listings retain IDs for project configuration and
  disambiguation. Local connection indexes identify networks and devices, not
  independently editable aliases. Duplicate names fail selection explicitly.
- Put server lifecycle operations under `tako network server`. Put client
  enrollment, listing and revocation under `tako network client`. `client add`
  creates an invitation; `network join` redeems it through the issuing server.
- App commands remain at the top level. Configuration stays file-based; this
  change does not introduce field-by-field app configuration commands.

## Invitation authority and device identity

An invitation contains pinned bootstrap information and a random single-use
capability. It is not a device private key. The requesting device creates and
persists independent SSH, WireGuard and secret-encryption keys before joining.
The enrollment proof binds its encryption public key along with its existing
identity keys.

The issuing server durably stores a hash of the invitation capability and the
granting administrator, scopes and deadlines. The first authenticated claim binds
the invitation to one enrollment request. Retries by that same request are safe;
another request cannot consume it again. Existing WireGuard possession checks
complete admission. The server rechecks the inviter's effective authority,
revocation and expiry at claim and completion.

An invitation is issued and redeemed by one server, so single-use behavior does
not depend on eventual replication. If that server is lost before admission,
create a new invitation. The invitation token is entered through a protected
prompt or stdin and must not appear in diagnostic logs. Membership approval is
distinct from OS reconciliation and readiness.

## Published secret records

Store secrets outside membership tables so their size, validation and retention
cannot compromise revocation processing. Address each value by permanent network
ID, app/environment, category and name. Categories distinguish application
secrets from administrator-only provider credentials, storage credentials and
backup keys.

Each accepted mutation has a stable operation ID, authenticated author, server
issuer and deterministic logical revision. A later observed revision wins;
concurrent revisions use a stable tie-breaker. This is per-value last-write-wins,
not a claim of global wall-clock ordering. Retain immutable revisions and
explicit deletion records. Retrying an uncertain publication reuses its original
operation ID and result, without promoting an old write above newer values.

Encrypt each value with a fresh random content key and authenticated context
binding its network, scope, category, name and operation ID. Wrap content keys
for authorized device encryption keys. Encryption-key envelopes and ciphertext
must remain bound to the same revision. Additional grants can distribute the
same content key to newly admitted authorized devices without changing the
value's revision. Servers can supply grants through the shared store; no direct
client exchange is required.

Full administrators and trusted servers can manage administrative credentials.
Scoped deployment clients can access application secrets for their granted
app/environments. Network membership alone does not grant secret access.
Recheck permissions on every read, write and new key grant. Revocation prevents
future grants and future versions from including the revoked identity; it cannot
erase previously decrypted values. Provider credentials themselves must be
rotated when their previous plaintext is no longer safe.

Use bounded input, record counts and response sizes, strict decoding, atomic
local persistence and restrictive file permissions. Keep private keys and
decrypted values out of serde debug output, logs and process arguments.

## Client cache and worktree drafts

- Device identity and authenticated shared snapshots can be reused by all
  worktrees on one machine. Draft mutations belong to one canonical project
  directory, app and environment.
- Secret edits stay local. Deploy refreshes membership and relevant published
  secrets, overlays only the selected worktree's actual edits, publishes those
  edits with stable operation IDs, and uses a fixed resolved snapshot for the
  release. It never republishes an entire stale cache.
- Client/server membership changes synchronize membership and access grants;
  they do not publish pending app secrets.
- Refresh preserves local edits and explicit deletions. A fresh checkout can
  read authorized secrets from its device's cache without copying another
  worktree's drafts. Cache updates never become accidental local writes.
- Missing/unavailable servers allow clearly identified cached inspection and
  local work. They do not acknowledge unpublished edits as replicated.
- Git merges do not transfer untracked secret drafts. In the merge-to-main
  workflow, new production secret values are configured in the release checkout.
  Deploying a branch targets the explicitly selected existing environment;
  there is no implicit branch preview or preview-resource provisioning.

## Migration and compatibility

Protocol v0 permits breaking producer/consumer changes. Update CLI, native
agents, bootstrap, policy adapters, test fixtures, MCP schemas and docs together.
Do not retain legacy command or inventory fallbacks.

Change generated ignore rules and repository examples so secret stores are not
tracked. Preserve existing encrypted values and local decryption keys during
one-time import into the new private worktree state. Do not silently regenerate
a missing key, drop credentials/backup keys, delete unpublished values, rewrite
Git history, or claim an old tracked file became untracked merely by changing
ignore rules. Document the explicit cleanup of existing tracked files after
publication is verified.

## Limits challenged during planning

- Reading one server cannot prove that it has received every concurrent update.
  Corrosion gives convergence, not a globally latest or linearizable read.
- Per-secret last-write-wins is intentional; unrelated names merge, same-name
  concurrent edits may lose. Deployment configuration is a complete validated
  snapshot from the checkout, not a per-field merge of agents' drafts.
- Existing checkout/server locks do not establish a network-wide transaction.
  Deployments to the same app/environment must be serialized by the release
  workflow; this change does not promise atomic multi-server deployment or
  introduce a new consensus service.
- Replication is not backup. Retained versions support inspection/recovery, but
  recovering from total server/key loss still requires protected backups.
- Secret availability on a newly added node depends on an authorized existing
  holder supplying its key grants. Missing keys must be visible, never treated
  as an empty or successfully synchronized secret set.

## Incremental delivery and verification

1. Shared identity and encryption primitives; deterministic secret revision,
   tombstone and envelope rules. Test cryptographic tampering, wrong recipients,
   identity binding, independent edits, concurrent edits and stale deletes.
2. Server storage/RPCs, invitation lifecycle, scoped access and key grants. Test
   expiry, revoked inviters, concurrent claim attempts, restart/lost-response
   retries, scope isolation, missing key grants and bounded storage.
3. Client caches/drafts and deploy integration, shared names and the command
   hierarchy; remove legacy inventory and Git-managed secret storage. Test two
   clients, two worktrees, migration with existing keys, pending deletions and
   failed publication retries.
4. Update native enrollment/deployment fixtures, documentation, canonical agent
   skills and generated discovery/schema outputs. Run affected Rust tests,
   `just test cli`, `just test e2e`, website checks and verification, and inspect
   the final diff. Record any real environment blockers in the PR.

The PR remains a draft until implementation and required verification are
complete. Commit and push each coherent milestone to the same branch.

## Implementation decisions confirmed

- Network names are immutable in this version; all devices read the descriptor's name.
- Enrollment and device key files move to version 2 to bind a separate encryption key.
  Older native identities require fresh enrollment, with existing app data and
  environment keys preserved. There is no rolling or in-place identity migration.
- Invitation lifetime defaults to one hour and is capped at 24 hours.
- Shared history is bounded at 10,000 records / 8 MiB, including grants and tombstones;
  automatic compaction remains outside this version.
- The real HTTPS integration test runs separate CLI processes for enrollment,
  permission checks, independent secret edits, lost responses, pending deletions,
  new worktrees, and revocation. A SQLite-backed Corrosion transport test exercises
  pagination, idempotent writes, conflicting payloads, and corrupt records.

## Verification findings

- Logical publication revisions include the client's highest observed revision,
  so editing through a lagging server does not place a new write behind a value
  already observed by that client. Retried operations keep their original revision.
- A scoped deploy inherits existing server-owned backup/runtime bindings rather
  than treating inaccessible administrative credentials as a request to clear them.
- Reading an existing worktree draft does not rewrite it; this avoids a dev
  watcher restart loop. Local edits still trigger reloads through the new filename.
- Deployment revalidates its fixed release snapshot after the final sync, because
  concurrent publication can change secrets or remove credentials after preflight.
  Scoped clients continue to inherit server-provisioned administrative bindings.
- A real two-agent Corrosion test caught the required default on non-null payload
  columns, which ordinary SQLite accepts without a default. The corrected schemas
  load and preserve independent records and conflicting operation variants.
- Local verification passes network, CLI, native management, website, and live
  Corrosion checks. Full deployment verification reached native enrollment but
  the managed host kernel rejects WireGuard interface creation. Linux CI remains
  the deployment verification gate; do not claim that local E2E completed.
- CI builds the authentic native membership fixture for CLI tests and runs the
  two-agent test against the checksum-pinned Corrosion release. Minimal deployment
  images retain their signed-package HTTP bootstrap until CA certificates exist;
  managed-proxy builds use HTTPS with the supplied trust bundle.
- Administrator recovery must account for cascading delegation revocation:
  a replacement administrator invited by the retiring administrator still
  depends on that authority chain. Preserve administrator keys and verify an
  independent surviving authority before revocation.
- Native enrollment failures identify installation, identity bootstrap, or service
  startup and retain bounded stderr diagnostics without exposing identity stdout.
  Failed deployment fixtures collect the native service journal before cleanup.
  Secret-validation test assertions use fixed failure messages rather than
  dumping secret-derived diagnostics into CI logs.
- CI loads and probes the host WireGuard module in an isolated network namespace
  before testing restricted native services. The helper journals its redacted
  failures, distinguishing missing kernel support and denied host operations
  without logging private inputs or raw command output.
