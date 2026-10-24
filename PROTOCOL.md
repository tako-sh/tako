# Tako Runtime Protocol

This document records the small set of contracts shared by the CLI, server, and SDKs. Exact message fields and serialized shapes live in typed code and tests.

The protocol version is `0`. Breaking changes are allowed until protocol v1. Change every producer and consumer in the same update. Do not add compatibility shims.

Tako has one active protocol version. The CLI, server, and every deploy artifact must declare that exact version; compatibility does not negotiate version ranges or capability sets. Deploy artifacts record it as `protocol_version` in `app.json`.

Normal deploys check the CLI/server protocol before parallel deployment starts. After upload, the server checks the artifact again before preparing dependencies, running a release command, synchronizing workflows, or starting app processes. `--force` bypasses only these version gates so an operator can attempt an incompatible deploy; normal validation and readiness checks still apply.

Server upgrades run the candidate binary in check-only mode before reload. The candidate checks the invoking CLI version and every active release, then repeats the check after the new process is ready. A mismatch aborts the upgrade. If a failure happens after reload starts, Tako restores and restarts the previous binary before leaving upgrade mode. `tako servers upgrade --force` permits an incompatible attempt but does not disable readiness or rollback.

Before the controlled reload, the CLI writes a one-shot upgrade-owner marker under the server data directory. The candidate consumes it and preserves the matching persisted upgrade fence through state restoration and the post-readiness check. An ordinary reload has no marker, so it still clears a stale upgrade lock.

## Executable owners

| Contract                                            | Owner                                                                                              |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Protocol version, commands, responses, and payloads | [`tako-core/src/protocol.rs`](tako-core/src/protocol.rs)                                           |
| Command serialization                               | [`tako-core/src/protocol/tests.rs`](tako-core/src/protocol/tests.rs)                               |
| Process bootstrap envelope                          | [`tako-core/src/bootstrap.rs`](tako-core/src/bootstrap.rs)                                         |
| Native process environment                          | [`tako-core/src/instance_env.rs`](tako-core/src/instance_env.rs)                                   |
| JSONL framing and limits                            | [`tako-socket/src/lib.rs`](tako-socket/src/lib.rs)                                                 |
| Native readiness handshake                          | [`tako-server/src/instances/spawner/readiness.rs`](tako-server/src/instances/spawner/readiness.rs) |

These sources win if this overview drifts.

## Transport boundaries

Tako has two control paths with different trust boundaries.

- The management path handles deploys and operator commands. Local server control uses the management Unix socket. Remote management exposes the same operations through signed HTTP requests on the configured private management address.
- The internal path handles workflow RPCs and server-side channel publishing. Each command carries its app identity. The production Unix socket authenticates the connecting peer's Unix identity against that app; the bootstrap token authenticates internal HTTP probes, not these socket commands.

The Unix transports use newline-delimited JSON. `tako-socket` caps one frame at 1 MiB. A connection can carry successive request/response pairs; SDK clients may pool and reuse connections. Streaming responses are defined by the owning API.

Management-only commands are rejected on the internal socket. Runtime-only commands are rejected on the management socket. Both paths share `tako_core::Command` so message shapes cannot drift between duplicate enums.

## Process bootstrap

Native app and worker processes receive the bootstrap envelope as JSON on file descriptor 3. The envelope contains the app-scoped internal token, decrypted secrets, and storage bindings. `tako-core::bootstrap` is the only server-side serializer for this shape.

Native HTTP processes receive `HOST=127.0.0.1` and `PORT=0`. The SDK binds an available loopback port, completes any readiness hook, and writes the resolved port to file descriptor 4. The server does not route traffic to the process before this handshake completes.

Tako-managed processes also receive `TAKO_APP_NAME` and `TAKO_INTERNAL_SOCKET`. SDKs require those variables as a pair before making workflow or channel RPCs.

Container processes receive the same bootstrap envelope through `TAKO_BOOTSTRAP_DATA`. HTTP containers bind `HOST=0.0.0.0` and `PORT=3000`; they do not use file descriptors 3 and 4. Container workflow processes receive the internal socket mount and the same app identity contract.

## Health probes

The server probes `GET /status` with `Host: <app>.tako` and an `X-Tako-Internal-Token` header. Tako SDK adapters own this internal endpoint. A healthy response must be successful and echo the token header so an unrelated public `/status` route cannot satisfy the probe.

The token is infrastructure-only. SDKs must not expose it to application code or public responses.

## Presence

JavaScript, Go, and Rust presence definitions use the channel registry and authorization endpoint, with wire names prefixed by `tako.presence:`. Channel and presence names therefore have separate namespaces. The connect authorization result supplies the trusted `subject`; it must succeed before the server creates a presence session or returns membership.

Presence uses the channel WebSocket route. After authorization, the server sends `{ "type": "tako.presence.ready" }`. The client then sends `{ "type": "tako.presence", "data": ... }` to join or replace its metadata. Client updates cannot choose a subject or connection ID. Metadata is JSON and is limited to 16 KiB.

Server snapshots are `{ "type": "tako.presence", "connectionId": "...", "members": [{ "connectionId": "...", "subject": "...", "data": ... }] }`; anonymous entries omit `subject`. These control frames have no replay ID and are never stored as channel messages. The React hook waits for `ready` before joining so its update cannot precede a browser's first-frame auth envelope.

Membership is scoped by deployed app/environment, wire name, and canonical validated params. Every authorized connection within one scope sees the same members. Params are serialized with recursively sorted object keys, then the wire name and params tuple is hashed with SHA-256 to produce a fixed-size storage key; query ordering does not create separate spaces. Each connection gets a server-generated ID, including separate tabs for one subject.

The client renews a 45-second lease with an update every 15 seconds. Disconnect cleanup removes its record; expiry covers network loss and process crashes. Snapshots poll shared state at approximately one-second intervals. SQLite and Postgres store leases separately from replay, with Postgres keyed by app ID for cross-server visibility. Local dev uses the same WebSocket session loop and an in-memory store; local auth remains permissive as with channels.

## Changing the protocol

Update the typed owner, every producer and consumer, and the serialization or integration tests in one change. Update this document only when a cross-component boundary or invariant changes. User-facing behavior belongs in the website docs and SDK READMEs.
