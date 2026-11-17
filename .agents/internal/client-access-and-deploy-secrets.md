# Client access and server-side deploy secrets

Plan for simplifying network clients to two access levels, making a client name a
stable identity across keys, moving secret resolution for deploys onto servers,
and giving CI a reusable identity. The implementation lands with this note; where
it had to differ from the first draft, this note records what was built.

## Goal and trust model

- A network is the trust boundary. Servers already hold every secret: they join
  with full permissions (`tako/src/commands/server/native.rs:349`) and the key
  reconciler wraps every readable record for them
  (`tako-network/src/runtime/shared.rs:300`). Keep that, and say it in the docs.
  Isolate environments or people you do not fully trust with separate networks.
- One kind of client by default: **full**. A full client can do everything in
  the network. The word "administrator" disappears from the CLI and docs.
- One optional restriction: **deploy**, given with `--deploy app/env`. A deploy
  client releases code to those environments and nothing else. It is meant for
  CI and agent sandboxes.
- A deploy never carries secret values. Servers resolve them from the shared
  store, so a deploy client never needs, and never gets, a secret.

## Decisions

### Access model

- `Permissions { network_admin, server_admin, ssh_user, deployments }`
  (`tako-network/src/registry.rs:22`) becomes an access value: `Full` or
  `Deploy(scopes)`. `independent_admin` keeps its meaning (a recovery root with no
  approver) under a name without "admin".
- Full clients get the `tako` SSH shell. There is no `--ssh` flag. A full client
  can already add servers, invite clients and read every key, so SSH adds little.
- Only full clients and servers become WireGuard peers. Filter the peer projection
  (`tako-server/src/native_agent/projection.rs:64`) on access. `tako connect` and
  `tako ssh` fail early for deploy clients. Deploy clients use management over
  pinned HTTPS only, which needs no peer.
- Delegation: only full clients invite. A full client may invite full or deploy
  clients; a deploy client invites nobody.
- **Revocation does not cascade.** Revoking a client, or replacing its key, affects
  only that client; the clients it invited keep their access. A member is valid
  when its inviter was valid at the moment the member was approved: no revocation
  of the inviter with `created_at` at or before the member's `created_at`, the
  inviter not yet expired then, and the inviter's own approval valid in the same
  way. Revocations already carry `created_at`. `client list` shows who invited each
  client, and revoke output lists the clients the revoked one invited so they can
  be revoked too. Trade-off: a thief who invites a client with a stolen device
  before you revoke that device keeps the invited client until you revoke it as
  well. The invited list in the revoke output is how you find it.

### Secrets

- Every secret kind lives in its environment and is encrypted locally with that
  environment's key: app secrets, credentials, storage credentials and backup keys
  (`EnvironmentSecrets`, `tako/src/config/secrets.rs:27`). Credentials are typed
  secrets that Tako consumes and never passes to app code. Unchanged.
- On the network every environment-scoped kind is readable and writable by full
  clients and servers only. Deploy clients read only the non-secret database
  directory, as now (`RecordAddress::readable`, `tako-network/src/shared.rs:145`).
  The app-vs-credential split in `writable()` (`shared.rs:137`) goes away.
- Per-reader sealing stays. It enforces the rule above and makes revocation work
  without rotating a shared key.

### Deploy

- No command carries secret values. `Deploy`, `PrepareDeploy`, `PrepareRelease`
  and `RunRelease` stop carrying `secrets` (`PrepareRelease` has them today,
  `tako-core/src/protocol.rs:99`, for release commands and private dependency
  installs), `runtime_credentials`, storage access keys, backup keys and the
  Cloudflare token. Release preparation and release commands resolve the same
  pinned snapshot on the server, so a deploy client with no local secrets can run
  release commands and install private dependencies.
  They carry non-secret binding configuration from `tako.toml` (provider, bucket,
  endpoint, region, retention) plus a **snapshot digest**.
- Snapshot digest: a hash over the sorted `(address, record id)` winners,
  tombstones included, for the deploy's `app/env` scope. A full client publishes
  its drafts first (deploy already does, `tako/src/commands/deploy/mod.rs:441`),
  then asks the leader server (`mod.rs:587`) for the digest, passing the record
  versions its publication produced so the leader and every target wait for
  those writes instead of pinning older winners from a lagging server. Today
  `sync_environment` returns only the `SecretsStore` and `publish_changes`
  consumes the operation IDs, so the publication result is threaded through the
  deploy flow. A deploy client only asks for the digest.
- Each target server computes its own digest. If it differs it waits, bounded, for
  replication, then fails with "secrets are still replicating" or "secrets changed
  during deploy; run it again". The client cannot tamper with the digest to gain
  anything: a wrong digest only fails the deploy.
- With a matching digest the server decrypts with its own key, merges secret parts
  into the bindings, builds the app environment and stores the result as the
  last-applied copy (`state_store` secrets, runtime credentials, storages, SSL,
  backup).
- Deploy-time secret validation stays on full clients, which read names and
  expiry dates from their local state without decrypting
  (`validate_release_secret_snapshot`, `tako/src/commands/deploy/config.rs`).
  Servers also refuse what they cannot apply: a missing storage credential or
  backup key, and an expired app secret or credential, with the command that
  fixes it (`tako-server/src/secret_snapshot.rs`). Deploy clients get those
  errors without local state.
- Remove `UpdateSecrets`: secret changes apply on the next deploy. Keep
  `GetSecretsHash`, a hash of the last-applied values with no values in it;
  tests use it to check that a failed deploy restores them. The CLI no longer
  calls it.
- The leader pins with `SecretsDigest { app, published }`. The server's main
  process reads values through a local-only network agent command
  (`AgentCommand::Secrets`), accepted from root and the service account over
  the agent socket and never built by the HTTP relay; its reply carries values
  in a field the relay never forwards.
- Debug builds only: tests without a network agent put values in
  `<data dir>/test-secrets.json`. Release builds never read it.
- Database management commands (`CreateDatabase`, `ConfigureDatabaseBackup`)
  still carry their snapshot key and storage credentials. Only full clients
  send them.
- Rollback keeps today's semantics: old code with the server's last-applied
  secrets and bindings (`rollback_app`, `tako-server/src/operations/releases.rs:350`
  passes `None` for all of them). It does not read the shared store.
- Builds do not receive secret values today; nothing under `tako/src/build` reads
  them. The manifest's `secret_names` (`tako/src/commands/deploy/manifest.rs:27`)
  is not used by the server; drop it.

### Deploy clients

A deploy client may send exactly: `Hello`, `ServerInfo`, `CheckDeploySpace`,
`PrepareRelease`, `PrepareReleaseUpload` (and the artifact upload endpoint),
`PrepareDeploy`, `Deploy`, `RunRelease`, `FinalizeRelease`,
`CleanupPreparedDeploy`, `CleanupRelease`, `SnapshotDatabase { app }`,
`MigrateDatabase { app }`, `Status`, `ListReleases` and `Rollback`. Everything else
is denied, including `Scale`, `Start`, `Stop`, `Delete`, all backup commands,
workflow commands, `EnqueueRun`, `Signal` and logs. See the open question on logs
and rollback.

Today a scoped client may send every application command in its scope
(`classify`, `tako-server/src/native_management_policy.rs:27`, and
`authorize_command`, `:202`): it can download and restore backups, delete the app,
read logs, and replace the server's stored secrets directly with `UpdateSecrets`.
The new list closes all of that.

Logs are authorized as `Status` (`tako-server/src/native_management.rs:258`).
Give logs their own check so allowing `Status` does not allow logs.

A CI-only setup must work end to end, so deploy clients are not limited to
redeploying what a full client provisioned. The `STATE_REQUIRED` and preservation
rules for scoped deploys (`native_management_policy.rs:159`, `:169`, `:172`–`:178`,
`:291`, `:299`) go away: a deploy client may make the first deploy of its
`app/env` and may change its routes and the plain configuration of its bindings.
Secret parts of bindings still come only from the server's view of the shared
store. One guard applies to every client: a server rejects a route that another
app or environment already claims, so a staging deploy client cannot take over
production's domain.

On the CLI, a deploy client's `tako deploy` must not call `sync_environment`,
`load_secrets_for_environment`, `load_secret_key`, `ensure_secret_key_available`
or `ensure_backup_keys_for_env` (`tako/src/commands/deploy/mod.rs:291`, `:430`).
It must work with no `.tako/state.local.json` and no environment key.
`can_configure_bindings` (`tako/src/commands/helpers.rs:4`) becomes an access
check.

### Client names and lifecycle

- **A client name is the principal.** Each `client add` creates a new key
  generation: a new member with new keys. Revocation applies per generation and
  stays permanent for those keys, since denial also matches WireGuard and SSH
  fingerprints (`registry.rs:336`). Reactivation never un-revokes old keys.
- Store the name on the approval and member as a server-asserted field (`client`),
  required for laptops and absent for servers. Do not reuse `request.name`: it is
  part of the device-signed enrollment request, so the inviter cannot set it.
  New devices set `request.name` to the client name for tidiness.
- `tako network client add <name> [--deploy app/env]... [--identity] [--yes]`.
  The name is required and chosen by the inviter. It travels in the invitation,
  and the claim must match it. `tako network join` drops `--name`.
- Name states:
  - **New name:** create the client.
  - **Revoked name:** reactivate. No prompt. Access comes from the new flags.
    Clients the old generation invited stay revoked.
  - **Active name:** ask `<name> is active. Replace its key? The current key
    stops working. [y/N]`. `--yes` skips the prompt. A non-interactive run without
    `--yes` fails and names `--yes`. Confirming revokes the current generation when
    the new invitation or identity is created, so an old laptop or a leaked CI
    identity stops working at once. History continues under the same name.
    Example: a contractor who left without being revoked comes back two months
    later.
- One name holds one active key. A person with two machines uses two names, for
  example `alice-laptop` and `alice-desktop`. Document this.
- Replace and revoke refuse to target the client the CLI is running as, and
  refuse to leave the network with no active full client.
- `client revoke <name>`, with `rm` and `remove` as aliases, revokes the active
  generation. It also accepts a device ID, to target one generation such as a
  conflicting duplicate. Add `server revoke` beside `server drain` and
  `server resume`. Remove the top-level `tako network revoke`
  (`tako/src/commands/network.rs:68`), which duplicates both.
- Revoke and replace list the clients the revoked generation invited, in the
  prompt and in `--json`. They keep their access; the list is there so they can be
  revoked too.
- `client list` shows active clients by default: name, access, invited-by, and
  the active generation's device ID and creation time. `--all` also shows
  revoked and expired clients and replaced key generations, each with a status
  column (`active`, `revoked`, `expired`, `replaced`, `conflict`) and invited-by.
  `--json` returns a specialized object whose rows carry the same `status` field
  (`.agents/internal/cli-output.md`, Modes). Today the list only shows effective
  members (`tako/src/commands/network/client.rs`, `List`), so revoked clients
  vanish.
- History shows the client name across generations. `secrets list` already looks
  authors up across all members, revoked ones included
  (`tako/src/network/secrets.rs:258`); switch it from `request.name` to the client
  name. Release attribution, when added, does the same.
- Host recovery `sudo tako-server admin invite` (`tako-server/src/admin_cli.rs:15`)
  becomes `sudo tako-server client invite <name>` and creates a full client. The
  name must not be active. The recovered client can then replace the lost
  device's name.

### Identity for CI

- `client add <name> --identity` prints an identity instead of an invitation. It
  works for full and deploy clients. Without `--deploy` it prints a warning, also
  to tracing so `--ci` transcripts keep it (`warning` is pretty-only,
  `.agents/internal/cli-output.md`, Text Helpers).
- The identity is written to stdout as data, so
  `... --identity | gh secret set TAKO_CLIENT_IDENTITY --env production` works.
  Human output goes to stderr. With `--json` it is a field of the result object.
- Admission needs no new server API. The CLI generates keys
  (`LocalDeviceKeys`, `tako-network/src/keys.rs:14`), creates the invitation,
  claims it as the new device and answers the WireGuard possession challenge
  (`tako-network/src/challenge.rs:35`) in-process, since it holds every key.
  The approver is the issuing device, which `client list` shows.
- Contents (as built): the prefix `tako-identity-v1-` and base64url JSON with the
  joined device's active profile and its key file. The profile includes the
  membership snapshot and the server pins, so using an identity needs no network
  call before the first command; commands refresh membership from the servers as
  usual. Creating one fails above 48 KiB, the size of a GitHub secret.
  Keep all three keys even for deploy clients: the device ID derives from the SSH
  and WireGuard public keys (`tako-network/src/enrollment.rs:67`), and deploy
  clients get neither peers nor wraps, so the extra keys grant nothing.
- `client add --identity` redeems its own invitation in a private temporary state
  directory (`crate::paths::set_network_state_dir`), completes admission with
  `network::client::refresh`, and packages the result. `TAKO_CLIENT_IDENTITY`
  uses the same override for the whole command (`tako/src/network/identity.rs`).
- `TAKO_CLIENT_IDENTITY` is read when set. The CLI keeps the identity in memory,
  writes no keys under the Tako home, and skips the keychain (the existing
  `TAKO_IDENTITY_DISABLE_KEYCHAIN` behavior). `tako network join` accepts either an
  invitation or an identity, telling them apart by prefix; joining with an
  identity stores it like a joined device.
- Naming: `TAKO_CLIENT_IDENTITY` because the value is the client's whole
  identity, and Tako already says "identity" for device keys. `KEY` would collide
  with the per-environment encryption key (`tako secrets key export`). The flag is
  `--identity`: `--ci` is a global output flag and `--export` is already
  `tako network export`.

## Implementation steps

Each step updates the docs and skills it changes (`commands/sync-docs.md`,
`commands/sync-agents.md`), runs affected tests, `just test cli` and
`just test e2e`, and lands as its own commits.

1. **Access model and naming.**
   `tako-network/src/registry.rs` (access type, `validate`, `can_delegate`,
   `delegated_by`, `can_admin`), `tako-network/src/shared.rs` (`readable`,
   `writable`), `tako-network/src/invitation.rs` and `runtime/invitation.rs`,
   `tako-network/src/runtime.rs` (revocation checks), `tako-server/src/native_management_policy.rs`,
   `tako-server/src/native_management.rs`, `tako-server/src/native_agent/projection.rs`,
   `tako-server/src/admin_cli.rs`, `tako/src/commands/network.rs`,
   `tako/src/commands/network/client.rs`, `tako/src/commands/server/native.rs`,
   `tako/src/commands/helpers.rs`, `tako/src/management_http.rs`,
   `tako/src/network/local_access.rs`, `PROTOCOL.md`. Today the permission fields
   appear in about 210 non-test places (`server_admin` 67, `deployments` 57,
   `independent_admin` 35, `network_admin` 31, `ssh_user` 22).
2. **Client names.** Name field and uniqueness in `registry.rs` (next to the
   server-name check, `:471`), the conflict rule in `effective_members`, name in
   the invitation and claim check, `client add <name>` with reactivation and
   replace, `client revoke`, `server revoke`, `client list`, `join` without
   `--name`, recovery invite rename, author names in `tako/src/network/secrets.rs`
   and `tako/src/commands/secret/sync.rs:173`.
3. **Server-side secret resolution.** `tako-core/src/protocol.rs` (`Deploy`,
   `PrepareDeploy`, `PrepareRelease`, `RunRelease` fields; `SecretsDigest`;
   secret-free binding configuration; remove `UpdateSecrets`), `tako/src/commands/deploy/{mod,artifacts,manifest,remote,config}.rs`,
   `tako/src/commands/backups/mod.rs:135`, `tako-server/src/operations/{deploy,releases,dispatch,drain}.rs`,
   a server module that opens the scope's records and computes the digest
   (`tako-network/src/runtime/shared.rs`), and the policy's binding checks.
4. **Deploy clients are deploy-only.** Command allowlist and a separate logs check
   in the policy and `native_management.rs`; deploy CLI path without local
   secrets or keys. Secret records become full-only in `RecordAddress::writable`
   (`tako-network/src/shared.rs`). The CLI sends plain storage and backup bindings
   from every client; database bindings stay full-only because they need grants.
   The route guard is `RouteTable::claimed_elsewhere` (`tako-server/src/routing.rs`),
   checked on `PrepareDeploy` and `Deploy` in `tako-server/src/operations/dispatch.rs`;
   it matches exact route patterns, so a wildcard can still catch subdomains no
   other app claims.
5. **Identity.** Key serialization (`tako-network/src/keys.rs`), an identity module
   in `tako/src/network/`, in-process admission in `client add --identity`,
   profile loading from the environment, `join` accepting both strings.
6. **Sweep.** Blog posts that show `--admin`, `--deploy`, `client remove` or
   `admin invite` (`native-wireguard-networks-for-vps-apps`,
   `sync-deployment-secrets-with-tako-networks`, `migrate-tailscale-to-tako-wireguard`,
   `how-to-deploy-vps-apps-over-netbird-with-tako`, `how-to-deploy-docker-compose-to-a-vps`,
   `how-to-deploy-rust-axum-to-a-vps`, `self-hosted-app-monitoring-prometheus-vps`,
   `tako-vs-sidekick`), `README.md`, `tako/README.md`, `tako-network/README.md`,
   skills in `sdk/javascript/skills/` regenerated with
   `bun website/scripts/sync-agent-skills.ts`, and the MCP stdin description
   (`tako/src/commands/mcp/catalog.rs:169`). Add a CI recipe: an identity stored as
   a GitHub Environment secret limited to `main`.

## Migration and breaking changes

Protocol v0, no shims: CLIs and servers upgrade together, and state written by
earlier versions is not read.

- Registry rows must use the new `Permissions` shape and `independent_root`.
  Rows with the earlier four-field permissions or `independent_admin` fail to
  decode, so a network created by an earlier version is not readable.
- Every laptop member carries `client`; a laptop without one is invalid. There
  is no name derivation for earlier members.
- The local state is `.tako/state.local.json`. `.tako/secrets.local.json` is not
  read. Its drafts are bound to the network they were made for (`bind`,
  `tako/src/network/secrets.rs`), so they cannot be published to a new network:
  set secrets again.
- Upgrade: uninstall the servers with the earlier CLI (`tako network server
  uninstall`, which also removes their app data), upgrade the CLI and servers,
  create a new network, add the servers, invite clients and set secrets again.
- Removed: `--admin`, `--ssh`, `join --name`, `tako network revoke`,
  `network request`/`approve`/`import`, `UpdateSecrets`. `tako-server admin
  invite` became `tako-server client invite`.
- Secret changes no longer reach running apps without a deploy; there is no
  in-place secret update.

## Verification

1. Registry tests for decoding, delegation and SSH projection; policy
   tests per command and access level; CLI integration tests for add, list and
   revoke (`tako/tests/cli_integration/`); `e2e/cli/tests/network.test.ts`.
2. Name uniqueness, reactivation, replace (the old key is rejected on the next
   request), the self and last-full-client guards, invited listing, no cascade, and continuous
   "set by" history. A two-agent Corrosion test with concurrent same-name
   admissions on two servers that converges on one active generation.
3. Deploy with a pinned digest across two servers; wait and timeout paths;
   mismatch after a concurrent publish; a lagging leader that has not yet seen
   the client's publication waits for it rather than pinning older winners;
   release preparation and release commands use the pinned snapshot; a server still missing key grants reports
   that, not missing secrets; rollback uses last-applied values; the deploy
   request types carry no secret fields.
4. A table test of every `Command` variant for deploy access; logs denied; a deploy
   client deploys with no secrets file and no environment key; `connect` refused.
5. Identity round trip; deploy from a fresh home with only
   `TAKO_CLIENT_IDENTITY`, writing no keys; `join` with an identity; revoke and
   replace stop the identity.

## Risks found while planning

- **Concurrent same-name admissions.** Invitations are stored and redeemed on one
  server (`tako-network/src/runtime/invitation.rs`), so two servers can each admit
  `github-ci` before replication. Uniqueness checks at invitation creation and
  approval cannot see each other. Resolution in `effective_members`: among
  effective laptop members with the same client name (case-insensitive), the
  smallest `(created_at, device_id)` is active; the others are `conflict` and not
  effective. Every node computes the same answer. Either may have worked briefly
  before convergence. Servers are out of scope; their name check
  (`registry.rs:471`) has the same race.
- **No cascade.** A thief holding a stolen full device can invite a client before
  you revoke the device, and that client survives the revocation. Revoke output
  lists what the revoked client invited; revoke those too.
- **Self-revocation.** Revoking or replacing the client the CLI runs as would cut
  off the device doing it. Refused, as is revoking the last active full client.
- **Replace before redeem.** The old key is revoked when the invitation is
  created. If the invitation expires unused, the client has no active key until
  the next `client add`.
- **New servers lack keys.** The reconciler grants at most 32 record keys per pass,
  every 2 seconds (`tako-server/src/native_agent/controller.rs:76`), only from a
  server that can already open the record. At the 10,000-record cap a new server
  can take minutes. Deploys to it wait or fail with a message that says so.
- **Identity is a bearer credential.** Anyone holding it is that client, and two
  holders are indistinguishable. A full identity is the whole network. Mitigated
  by the warning, revocation and replace.
- **Deploy-only dry runs** cannot validate secrets; they say the server validates.
- **Go accessors** (`tako_secrets.go`) are generated from local secret names by
  `tako gen`, init and secret commands, not at deploy. CI with a deploy client must
  use the committed files rather than regenerate them.

## Questions decided

1. **Logs for deploy clients.** Recommend deny. Logs can contain secret values, and
   CI can report `Status` on failure. Allow later behind an explicit flag if needed.
2. **Rollback for deploy clients.** Recommend allow. It only reactivates a release
   already on the server, with last-applied values.
Answered:

1. **Logs for deploy clients:** denied.
2. **Rollback for deploy clients:** allowed; rollback counts as a deploy.
3. **First deploy and route changes by a deploy client:** allowed, so CI alone can
   run an app. Servers reject a route another app or environment already claims,
   for every client.
4. **Re-parenting after revocation:** not needed; revocation no longer cascades.
5. **`network request`, `approve` and `import`:** removed once identities exist.

## Later

- Record the deploying client on each release (`ReleaseInfo`,
  `tako-core/src/protocol.rs:1414` has no author field). The server can take it
  from the signed request.
- `client update` to change access without a new key.
- An access end date for clients (`--until`); members already have `expires_at`.
- GitHub OIDC for deploy clients, so servers check repository and branch
  themselves and no long-lived identity exists.
- Servers with less than full access.
