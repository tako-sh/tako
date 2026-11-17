# Client access and server-side deploy secrets

Plan for simplifying network clients to two access levels, making a client name a
stable identity across keys, moving secret resolution for deploys onto servers,
and giving CI a reusable identity. Nothing here is implemented yet.

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
  clients; a deploy client invites nobody. Revocation keeps cascading down the
  approval chain (`delegated_by`, `registry.rs:224`).

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

- The `Deploy` and `PrepareDeploy` commands stop carrying `secrets`,
  `runtime_credentials`, storage access keys, backup keys and the Cloudflare token.
  They carry non-secret binding configuration from `tako.toml` (provider, bucket,
  endpoint, region, retention) plus a **snapshot digest**.
- Snapshot digest: a hash over the sorted `(address, record id)` winners,
  tombstones included, for the deploy's `app/env` scope. A full client publishes
  its drafts first (deploy already does, `tako/src/commands/deploy/mod.rs:441`),
  then asks the leader server (`mod.rs:587`) for the digest, passing the
  operation IDs it published so the leader waits for them. A deploy client only
  asks for the digest.
- Each target server computes its own digest. If it differs it waits, bounded, for
  replication, then fails with "secrets are still replicating" or "secrets changed
  during deploy; run it again". The client cannot tamper with the digest to gain
  anything: a wrong digest only fails the deploy.
- With a matching digest the server decrypts with its own key, merges secret parts
  into the bindings, builds the app environment and stores the result as the
  last-applied copy (`state_store` secrets, runtime credentials, storages, SSL,
  backup).
- Deploy-time secret validation moves to the server. Today the client runs it
  (`validate_release_secret_snapshot`, `tako/src/commands/deploy/config.rs:371`;
  `validate_secrets_for_deployment`, `tako/src/validation/secrets.rs:47`). Servers
  can read every environment, so the cross-environment name check still works.
  Errors come back as the same messages.
- Remove `UpdateSecrets` and `GetSecretsHash`. The CLI no longer sends
  `UpdateSecrets`, and `GetSecretsHash` only feeds the hash skip in
  `tako/src/commands/deploy/remote.rs:578`, which the digest replaces.
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

Keep the existing preservation rules: a deploy client cannot change routes, SSL,
runtime credentials, storage, database bindings or backup
(`native_management_policy.rs:159`, `:172`–`:178`, `:291`, `:299`). It also still
needs provisioned state on each target server (`STATE_REQUIRED`, `:169`), so the first
deploy of an `app/env` to a server, and any route or binding change, comes from a
full client.

On the CLI, a deploy client's `tako deploy` must not call `sync_environment`,
`load_secrets_for_environment`, `load_secret_key`, `ensure_secret_key_available`
or `ensure_backup_keys_for_env` (`tako/src/commands/deploy/mod.rs:291`, `:430`).
It must work with no `.tako/secrets.local.json` and no environment key.
`can_configure_bindings` (`tako/src/commands/helpers.rs:4`) becomes an access
check.

### Client names and lifecycle

- **A client name is the principal.** Each `client add` creates a new key
  generation: a new member with new keys. Revocation applies per generation and
  stays permanent for those keys, since denial also matches WireGuard and SSH
  fingerprints (`registry.rs:336`). Reactivation never un-revokes old keys.
- Store the name on the approval and member as a server-asserted field (`client`),
  required for laptops and absent for servers. Do not reuse `request.name`: it is
  part of the device-signed enrollment request, so a migration cannot rename it.
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
- Replace and revoke refuse to target the client the CLI is running as, or any
  client in its approval chain. Revoking either would cut off the running device,
  and with it the new generation, which it approved.
- `client revoke <name>`, with `rm` and `remove` as aliases, revokes the active
  generation. It also accepts a device ID, to target one generation such as a
  conflicting duplicate. Add `server revoke` beside `server drain` and
  `server resume`. Remove the top-level `tako network revoke`
  (`tako/src/commands/network.rs:68`), which duplicates both.
- Revoke and replace list the clients that lose access with it (everything that
  generation invited, recursively) and include them in the prompt and in `--json`.
- `client list` shows one row per client: name, access, status (`active`,
  `revoked`, `expired`, `conflict`) and the active generation's device ID and
  creation time. `--json` returns a specialized object with each client's
  generations (`.agents/internal/cli-output.md`, Modes). Today the list only
  shows effective members (`tako/src/commands/network/client.rs`, `List`), so
  revoked clients vanish.
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
  The approver is the issuing device, which matters for the revocation cascade.
- Contents: a versioned prefix and base64url JSON with the network ID and name,
  client name, device ID, the three private keys (SSH signing, WireGuard,
  encryption) and up to three servers with HTTPS endpoint and certificate pin.
  Keep all three keys even for deploy clients: the device ID derives from the SSH
  and WireGuard public keys (`tako-network/src/enrollment.rs:67`), and deploy
  clients get neither peers nor wraps, so the extra keys grant nothing. Do not
  embed the membership snapshot; refresh it from the listed servers on use.
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
   `PrepareDeploy`, `BackupNow` fields; secret-free binding configuration; remove
   `UpdateSecrets` and `GetSecretsHash`), `tako/src/commands/deploy/{mod,artifacts,manifest,remote,config}.rs`,
   `tako/src/commands/backups/mod.rs:135`, `tako-server/src/operations/{deploy,releases,dispatch,drain}.rs`,
   a server module that opens the scope's records and computes the digest
   (`tako-network/src/runtime/shared.rs`), and the policy's binding checks.
4. **Deploy clients are deploy-only.** Command allowlist and a separate logs check
   in the policy and `native_management.rs`; deploy CLI path without local
   secrets or keys.
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

Protocol v0, no shims. Servers and CLIs upgrade together.

- Each server migrates its registry rows deterministically, so replicated rows
  match: `network_admin && server_admin` becomes full; scopes without both become
  deploy; `ssh_user` is dropped. Client caches are discarded and fetched again.
- Existing full members without `--ssh` gain SSH. Call it out in release notes.
- Existing laptops get `client` = `request.name`. Names default to the hostname
  (`tako/src/commands/network.rs:387`), so duplicates exist. The oldest member
  keeps the name; the others get `-` plus the first six characters of their device
  ID. This is a one-time migration, not a runtime rule.
- Existing `--deploy` clients lose app-secret access, connect, logs, backups,
  scale and delete. Agent sandboxes that set secrets need a full client.
- Removed: `--admin`, `--ssh`, `join --name`, `tako network revoke`,
  `tako-server admin invite`, `UpdateSecrets`, `GetSecretsHash`.

## Verification

1. Registry tests for the migration mapping, delegation and SSH projection; policy
   tests per command and access level; CLI integration tests for add, list and
   revoke (`tako/tests/cli_integration/`); `e2e/cli/tests/network.test.ts`.
2. Name uniqueness, reactivation, replace (the old key is rejected on the next
   request), the self and ancestor guards, cascade listing, and continuous
   "set by" history. A two-agent Corrosion test with concurrent same-name
   admissions on two servers that converges on one active generation.
3. Deploy with a pinned digest across two servers; wait and timeout paths;
   mismatch after a concurrent publish; a server still missing key grants reports
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
  smallest `(created_at, device_id)` is active; the others are `conflict`, not
  effective, and their descendants lose access through the chain. Every node
  computes the same answer. Either may have worked briefly before convergence.
  Servers are out of scope; their name check (`registry.rs:471`) has the same
  race.
- **Revocation cascades.** Replacing or revoking a client also cuts off everything
  that generation invited, including CI identities it minted. Revoking a lost
  laptop can silently stop CI. The listing in the prompt is the mitigation.
- **Self-revocation.** Replacing your own client, or one in your approval chain,
  would revoke the device doing it and the new generation it approved. Guarded.
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

## Open questions

1. **Logs for deploy clients.** Recommend deny. Logs can contain secret values, and
   CI can report `Status` on failure. Allow later behind an explicit flag if needed.
2. **Rollback for deploy clients.** Recommend allow. It only reactivates a release
   already on the server, with last-applied values.
3. **First deploy by a deploy client.** Recommend keeping the requirement that a
   full client deploys first and makes route or binding changes. The alternative,
   servers provisioning routes from a deploy client's `tako.toml`, reopens route
   hijacking across apps.
4. **Re-parenting after revocation.** Recommend listing dependents now and deciding
   later whether another full client can re-approve them.
5. **`network request`, `approve` and `import`.** Their main use, enrolling CI
   runners, moves to identities. Recommend removing them in step 5 unless another
   use appears.

## Later

- Record the deploying client on each release (`ReleaseInfo`,
  `tako-core/src/protocol.rs:1414` has no author field). The server can take it
  from the signed request.
- `client update` to change access without a new key.
- An access end date for clients (`--until`); members already have `expires_at`.
- GitHub OIDC for deploy clients, so servers check repository and branch
  themselves and no long-lived identity exists.
- Servers with less than full access.
