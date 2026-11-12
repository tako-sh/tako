# Tako Runtime Protocol

This document records the small set of contracts shared by the CLI, server, and SDKs. Exact message fields and serialized shapes live in typed code and tests.

The protocol version is `0`. Breaking changes are allowed until protocol v1. Change every producer and consumer in the same update. Do not add compatibility shims.

Tako has one active protocol version. The CLI, server, and every deploy artifact must declare that exact version; compatibility does not negotiate version ranges or capability sets. Deploy artifacts record it as `protocol_version` in `app.json`.

Deploy artifacts may also record the Tako SDK the app is built against as `sdk` in `app.json`: `{ "language", "name", "version" }`, where `language` is `javascript`, `go`, or `rust`. The CLI reads it from the app's dependency files at deploy time: the installed or locked `tako.sh` package, the `tako.sh` requirement in `go.mod` (`(devel)` when a `replace` points at a local directory, or the replacement module and version), or the `tako` crate in `Cargo.lock`. It is omitted when no SDK is found, including prebuilt images, Compose projects, and containers with `container_options.sdk = false`. The server reports it in `list_releases` and never gates on it.

Normal deploys check the CLI/server protocol before parallel deployment starts. After upload, the server checks the artifact again before preparing dependencies, running a release command, synchronizing workflows, or starting app processes. `--force` bypasses only these version gates so an operator can attempt an incompatible deploy; normal validation and readiness checks still apply.

`PrepareRelease` carries the current deployment's secrets for preparation, including Compose interpolation on a first deploy. Preparation does not replace persisted app secrets; activation persists them after preparation succeeds.

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

## Internal socket error replies

Every reply is `tako_core::Response`: `{"status":"ok","data":...}` or `{"status":"error","message":"...","code":"..."}`. `message` is free text for logs and is not a contract. `code` is present only where an SDK or app must branch; a reply without it is unclassified, and management replies never carry one. Readers treat a code they do not know as unclassified. Codes are the entry names of the `socket` scope in `errkit.json` (generated into `tako-core/src/error_codes.rs`, named by `tako_core::ErrorCode::name`):

| `code`                   | Meaning                                                                                                                                                            | Replies to                                                                                            |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| `run_not_owned`          | The run is no longer owned by this claim (lease reclaimed, cancelled, finished).                                                                                   | `heartbeat_run`, `save_step`, `complete_run`, `cancel_run`, `fail_run`, `defer_run`, `wait_for_event` |
| `run_id_conflict`        | The supplied run ID belongs to a different workflow or payload.                                                                                                    | `enqueue_run`; network `enqueue`                                                                      |
| `invalid_request`        | The command is malformed, not accepted on this socket, or has an invalid run ID, address, channel payload, or close target. Retrying cannot succeed.               | any command; network requests                                                                         |
| `channel_unknown`        | The publisher's loaded build, or the recipient app, does not define the channel.                                                                                   | `channel_publish`, `channel_close`; network `publish`                                                 |
| `channel_params_invalid` | Params are not a flat object of at most 16 KiB, name a null or reserved param, or fail the params schema.                                                          | `channel_publish`, `channel_close`; network `publish`                                                 |
| `channel_closed`         | The stream has a retained close marker.                                                                                                                            | `channel_publish`; network `publish`                                                                  |
| `unavailable`            | Draining, an unhealthy worker, network or publisher overload, an unreachable destination, the stream limit, or a busy or disconnected workflow store. Retry later. | `enqueue_run`, `network_call`, `channel_publish`, `channel_close`, workflow commands                  |

SDKs decide lost ownership only from `run_not_owned` on a worker write, never from message text: other messages, such as the `run_id_conflict` one, echo caller-chosen values. A network recipient replies with the same shape; a rejection from its own internal socket keeps its code, not its message. JavaScript exposes the code as `TakoError.reason` (with `code: "TAKO_RPC_ERROR"`), Go as `*ServerError.Code` matched by sentinels such as `ErrRunNotOwned` with `errors.Is`, and Rust as `tako::ErrorCode` (`ErrorCode::of(&io::Error)` for network calls, `StoreError::Rejected(Option<ErrorCode>)` for Store invalidations).

## Network primitives

`network_call` is an internal-socket command with `{app, address, request}`. `app` remains the authenticated caller identity. `address` is a bare primitive name or `namespace.primitive`; a qualified namespace selects another app in the caller's environment. URLs, paths, extra dots, and caller-selected environments are rejected. `tako-core/src/network.rs` owns the invocation shapes and address grammar.

Requests are `call` with JSON input for direct handlers, `enqueue` with payload and options, `result` with a run ID, or `publish` with message and params. Direct handlers run without enqueueing and return JSON. The recipient supplies workflow retry defaults or channel lifecycle settings. Enqueue acknowledges a run ID; publish acknowledges the stored or ephemeral message. Result reads return null for a missing workflow/run pair, otherwise `{status,result,error}`. Result reads are short requests; clients poll rather than holding routing capacity while a workflow runs. Network delivery is attempted once. A separate client socket pool reserves storage capacity, and both client and server reject network overload immediately so nested handlers cannot queue behind waiting ancestors.

For native networks, the router selects an authenticated placement and sends `POST /_tako/network` over the private mesh. The body carries an agent-signed invocation under `tako-network-invocation-v1`, binding network, source server, destination server, caller app/environment, target app/environment, issuance time, nonce, and invocation. The caller comes from authenticated socket credentials. Protected local agent IPC signs the proof; proxies do not read identity keys. Receivers require a fresh admitted server identity matching the actual WireGuard peer, verify destination and signature, and reject expiry or replay. Generic enrolled laptops and Compose peers cannot invoke this path. Proofs cannot be relayed to another placement. Public ingress rejects it. Only after verification does the proxy wake the app and send `POST /network` with its internal host/bootstrap token; the response must echo the token. Local development and non-network same-server routing use that recipient endpoint directly. Requests and responses are bounded to 1 MiB; redirects and environment HTTP proxies are disabled. Management endpoints reject `network_call`.

JavaScript implements recipient dispatch, including flat `<app_root>/network-handlers/` functions addressed by filename. JavaScript, Go, and Rust implement socket callers. `tako gen` exports `tako-schema.json` (JSON Schema 2020-12 with version 1 `x-tako` app/primitive metadata). Root `schemas` imports local paths or HTTP(S) URLs. A committed `tako-schema.lock` embeds hashed remote schemas; updates are explicit. Consumers compile standalone TypeScript contracts (`tako.imports.ts`), Go DTOs (`tako_imports.go`), or Rust Serde DTOs (`tako_imports.rs`). Go DTO and Rust schemars exporters describe native JSON contracts without adding recipient dispatch. `tako.exports.d.ts` remains a JavaScript convenience export. Generation changes no package exports. Type imports do not alter network authorization or runtime registration.

## Process bootstrap

Native app and worker processes receive the bootstrap envelope as JSON on file descriptor 3. The envelope contains the app-scoped internal token, decrypted secrets, and storage bindings. `tako-core::bootstrap` is the only server-side serializer for this shape.

Native HTTP processes receive `HOST=127.0.0.1` and `PORT=0`. The SDK binds an available loopback port, completes any readiness hook, and writes the resolved port to file descriptor 4. The server does not route traffic to the process before this handshake completes.

Tako-managed processes also receive `TAKO_APP_NAME` and `TAKO_INTERNAL_SOCKET`. SDKs require those variables as a pair before making workflow or channel RPCs.

Container processes receive the same bootstrap envelope through `TAKO_BOOTSTRAP_DATA`. HTTP containers bind `HOST=0.0.0.0` and `PORT=3000`; they do not use file descriptors 3 and 4. Container workflow processes receive the internal socket mount and the same app identity contract.

Container release artifacts carry `container_options`, including the container port, persistent data mount, optional numeric user mapping, and SDK mode. The host app data directory is mounted at the configured container path and `TAKO_DATA_DIR` is translated to that path, for HTTP containers and container workflow processes alike. Plain containers (`sdk = false`) receive no bootstrap token and use an ordinary HTTP health probe. Only explicitly selected secrets become environment variables. SDK containers retain the authenticated `/status` contract during both startup and steady-state health checks.

Compose releases carry `release_kind = "compose"` and the shared `ComposeConfig`. They represent one project on one server and use plain HTTP ingress health checks. The server prepares immutable service images without persisting interpolated secrets. Project identity is derived from the persistent app data path, independent of release version. Production creates immutable service generations with stable network aliases and resources; fingerprints compare resolved configuration and image IDs. Unchanged containers are adopted. The selected stateless HTTP service defaults to start-first replacement with readiness and draining; writable persistent volumes and other services default to stop-first. `deploy.update_config.order` and `monitor` control per-service updates. A pending marker precedes mutations; active release, routes, and encrypted credentials commit atomically. Recovery reconciles the committed release and cleans abandoned generations. HTTP instance cleanup owns only its container, while explicit app stop owns the project. Named volumes survive stop and deletion and remain outside Tako app-data backups. Referenced release files are protected from pruning. `AppStatus.services` reports original service names, state, health, and exit codes; `services_error` reports query failures.

## Health probes

The server probes `GET /status` with `Host: <app>.tako` and an `X-Tako-Internal-Token` header. Tako SDK adapters own this internal endpoint. A healthy response must be successful and echo the token header so an unrelated public `/status` route cannot satisfy the probe.

The token is infrastructure-only. SDKs must not expose it to application code or public responses.

`AppStatus.recovery` is present only while the server has replaced instances since the last deploy, scale, or stable minute. It carries `restart_count`, `last_exit` (`reason` of `crashed`, `unhealthy`, or `failed_to_start`; the observed `exit_code` or `signal` when the process exited by itself; `at_unix_secs`), and `next_restart_at_unix_secs` while replacement is backing off.

## Workflow claim ownership

`ClaimRun.worker_id` identifies a worker process for concurrency accounting.
Every successful claim returns a fresh opaque `RunPayload.worker_id`. The worker
must echo that returned identifier on all heartbeats, checkpoints, and lifecycle
mutations. Reclaiming a run produces a different owner even within the same
process, so a delayed earlier attempt cannot mutate the new attempt's state.
SDK process identities also include randomness to avoid PID reuse across hosts
or restarts. Ownership checks fence database writes, not external side effects;
workflow execution remains at least once.

## Workflow results and child calls

`CompleteRun` carries a JSON `result` (null for no return value). The store atomically commits it with the succeeded status, guarded by the current claim owner. SQLite and Postgres persist it in `runs.result` and remove it with retained run history. `GetRunResult {app,name,id}` is an authenticated runtime-socket command; a mismatched workflow name is a missing run, and the app/environment scope is enforced by the same peer authentication as enqueue. It is not a management command.

JavaScript `.run()` combines enqueue and result waiting. In a workflow, the SDK saves a child ID and call identity before enqueueing, repeats an uncertain enqueue with that ID, and checkpoints the terminal outcome. Pending children use ordinary `DeferRun`, releasing the parent's claim/concurrency slot without spending a retry. Result waits checkpoint their deadline across replay; a timeout does not cancel the child. Named `ctx.run` steps scope child-call order. IDs remain deduplicated only while child history is retained. Detached `.enqueue()` and direct handler `call()` do not gain automatic workflow checkpoints.

## Workflow schedules

A starting worker sends `RegisterSchedules {app,schedules}`, which replaces the app's schedule set. Each `ScheduleSpec` is `{name,cron,max_attempts?}`. `max_attempts` is the workflow's attempt budget, counting the first attempt: JavaScript sends `retries + 1` and Go sends `WithMaxAttempts`. The store keeps it with the schedule, and every run the schedule creates gets it. Omitted, scheduled runs get the default of three attempts.

## Workflow run operations

Operators inspect and repair runs with four management commands: `ListWorkflowRuns {app,status?,name?,limit?}`, `GetWorkflowRun {app,id}`, `RetryWorkflowRun {app,id}`, and `CancelWorkflowRun {app,id}`. They are app-scoped like other application commands; list and get are read-only. The internal socket rejects them, so an app process cannot list or change runs, its own included. The worker's `CancelRun` is a different command: it carries the claim owner and ends the run from inside its handler.

Each server answers from the app's store as it sees it: its own SQLite file, or the Postgres store shared by every server of the app. Replies name the store (`sqlite` or `postgres`) so clients can tell one server's runs from a shared set. A stopped app's runs stay readable.

List returns the newest runs first, 50 by default and at most 200. Get returns every run field, the 100 most recent saved steps, and the run's event waits. Replies must fit one 1 MiB JSONL frame, so the server caps stored text: the payload and result at 32 KiB, each step result at 2 KiB, and errors at 8 KiB (1 KiB in lists). Workflow, step and event names and claim owners are cut to 256 bytes. A reply serializes to at most 768 KiB: a list stops early and sets `has_more`, and a run drops its oldest steps. A capped JSON value carries `truncated: true`, its stored size, and a UTF-8-safe text preview instead of the value. A run parked until a signal reports no due time.

Retry accepts only `dead` and `cancelled` runs. The run becomes `pending`, due now, with its attempts reset and its saved steps kept, so the handler resumes after its last checkpoint. Cancel accepts `pending` and `running` runs and removes their event waits. A running attempt loses its claim: its later heartbeats, checkpoints, and lifecycle writes are rejected with the same `run_not_owned` error as a reclaimed attempt's, and reclaim leaves the cancelled run alone. SDK workers treat that code on a worker write as lost ownership. They stop the attempt at its next checkpoint without failing, retrying, or completing the run; side effects already started are not undone.

## Ephemeral channels

Channel registry metadata, authorization responses, and internal publish lifecycle metadata carry optional `ephemeral: true`. Omission or false selects durable delivery. Ephemeral definitions omit `replayWindowMs`; supplying both is invalid. Durability is a definition setting and cannot be changed by client input or an authorization grant.

Ephemeral publication uses a bounded process-local broadcast bus shared by the publisher and proxy stores for the same deployed app database path. It does not write channel metadata or message rows. Only active subscriptions receive events; lagging subscribers skip evicted events and reconnects have no replay. Ephemeral `ChannelMessage` values have an empty `id`; SSE frames omit the replay ID. Shared Postgres stores reject ephemeral delivery until a transient cross-server transport is available. Local development reads the channel registry for params and the auth scheme and takes the delivery mode from the app's authorize answer, as production does.

## Channel streams

A channel stream is identified by the channel name and its validated params. Subscribers, WebSocket connections, client WebSocket publishes, replay, cursors, budgets, and fan-out are per stream, for durable and ephemeral channels, on one server and across the mesh or a shared Postgres store. Absent params and an empty object select the channel's single unparameterized stream. Presence keeps one transport stream per presence name; its membership is scoped by params separately.

Subscribe and connect requests carry params as flat query pairs on `/_tako/channels/<name>`, excluding the reserved WebSocket cursor key `last_message_id`. The server coerces each value by the type its params schema declares for that property (`integer`, `number`, `boolean`; anything else stays a string) and validates the result against the schema, in production and local development. Each distinct param value selects a stream, so a param the schema does not declare under `properties` is rejected (`400`, `invalid channel params: unknown param '<name>'`, before authorization and before any stream bookkeeping) unless the schema explicitly sets `additionalProperties: true`, whatever JSON Schema's own default; a schema without properties accepts no params. The reserved query keys below are not params and are unaffected. WebSocket requests are checked the same way before the upgrade. The JavaScript `defineChannel`/`definePresence`, Go `Channels.Register`, and Rust `PresenceDefinition` emit `additionalProperties: false` when no schema (or, in JavaScript, no explicit setting) is given; Store schemas are unchanged. Canonicalization matches Stores and presence: object keys are sorted, null entries are dropped, and integral numbers compare equal (`1` and `1.0`).

The internal `channel_publish` command is `{ "command": "channel_publish", "app", "channel", "params", "build", "payload" }`. `channel` is the public name. `params` is a JSON object, at most 16 KiB, or omitted/null for the unparameterized stream; values must have the types subscribers' query values are coerced to. A query cannot carry null, so a `null` param value is rejected (`channel param '<name>' is null`) rather than dropped: dropping it would select a different stream, the unparameterized one if it was the only param. `build` is the publisher's `TAKO_BUILD`. When the server has loaded the channel registry for that build, it rejects a channel that build does not define (`channel publish failed: bad request: channel '<name>' is not defined by build <build>`; the same for `channel_close`), validates `params` against the channel's params schema, including the undeclared-param rule, and rejects mismatches; without a loaded registry only the shape is checked; it never loads the registry for a publish. Stores ignore `params` and keep scoping invalidations by `payload.scopeParams`. The acknowledgement and every delivered `ChannelMessage` carry the public channel name; the internal stream key is never sent to clients or apps. Names starting with `tako.stream:` are reserved and rejected as routes and publish targets.

Channel params are flat scalars. The JavaScript SDK serializes each subscribe/connect param as a query value (dropping `undefined` and null, rejecting objects, arrays, and non-finite numbers) and, for a publish, sends what the server would bind for that query: each value is serialized the same way, then coerced by the definition's declared property type. A value a JavaScript number cannot carry exactly would bind different streams on the two sides, so with a definition the SDK rejects, when a handle is bound and again on subscribe, connect, publish, and close, an `integer` param that is not a safe integer (magnitude above 2^53 - 1, number, `bigint`, or numeric string) and a `number` param that is not finite (such as `1e400`). `channel_publish` omits `params` for the unparameterized stream and `build` when `TAKO_BUILD` is unset. A network `publish` request's `params` are validated against the recipient's definition and select the stream as a local publish does. Go has no server-side channel publish.

Replay cursors are opaque strings that name their stream (SQLite `s1.<epoch>.<stream>.<position>`, Postgres `p1.<stream>.<position>`), so a cursor from one params binding, or a bare numeric position, is stale on another stream and the client resets. Stream bookkeeping is released once a stream retains no messages and has had no activity for longer than its longest connection lifetime plus a grace period; a released stream restarts at the store's current head, so cursors from its earlier life reset. An app keeps bookkeeping for at most 100,000 streams (the default total replay message budget). Starting another stream derived from params (a channel stream or a Store scope) beyond that fails with a logged storage error (`500` for a subscribe, `channel publish failed: storage error: channel stream limit reached ...` for a publish); existing streams and unparameterized channel names are never refused, and a mesh event for a new stream at the limit is acknowledged and dropped. On shared Postgres, concurrent subscribers on several servers can overshoot the limit by the streams they start at once. Maintenance runs every second with bounded work: age pruning visits at most 1,000 streams per tick, continuing after the last one next tick, and at most 1,000 idle streams are released per tick, both through indexes. Readers compute expired history themselves, so late pruning never serves expired messages.

Subscribe and connect requests may add the reserved query pair `from=start` to read every retained message of the stream instead of starting at its head. Any other `from` value is rejected with 400. An explicit cursor (`Last-Event-ID` for SSE; the `last_message_id` query or `tako.auth` `lastMessageId` for WebSocket) wins over `from`. If retention already dropped the beginning, the reader gets what is retained, without a reset. Ephemeral streams retain nothing, so `from=start` reads live messages only. `from` and `last_message_id` are never channel params: they are stripped from the params query (after percent-decoding), and publish or close params naming them are rejected.

The internal `channel_close` command is `{ "command": "channel_close", "app", "channel", "params", "build", "lifecycle" }`. `channel`, `params`, and `build` select and check the stream exactly as for `channel_publish`; `lifecycle` is the definition's lifecycle in the shape of a publish payload's `lifecycle` and may be omitted. It appends a durable terminal marker, a row of the reserved type `tako.close`, and replies `{ "id", "channel", "type": "tako.close", "data": null }` with the public channel name (`eventId` as for publish acknowledgements). Closing a closed stream succeeds and returns the existing marker. Ephemeral channels, Stores, and presence spaces cannot be closed (`channel close failed: bad request: ...`). While the marker is retained, `channel_publish` to that stream fails with `channel publish failed: channel stream is closed` and leaves the stream untouched (its lifecycle and activity time are applied only with an accepted append, so rejected publishes cannot keep an inactivity-TTL stream alive); no publish payload, server-side or WebSocket client, may use the type `tako.close`. The marker replicates over the mesh and through shared Postgres like a message, survives restarts, and is never delivered as an application message. Close wins on every SQLite node: once a stream has a retained marker, ingest acknowledges and drops any later event for it, a publish or a second marker, without storing or delivering it; the origin's sequence still advances, so the drop is no replication gap. A close is therefore ordered after its own server's earlier publishes, which reach peers first, while a publish from another server that races the close may be dropped (servers that applied it before the marker keep it). Shared Postgres takes publishes and closes under one per-app lock, so nothing races there. Each durable mesh event (`ChannelPublication::Event`) carries an optional `lifecycle` (`replayWindowMs`, `inactivityTtlMs`, `keepaliveIntervalMs`, `maxConnectionLifetimeMs`): the origin stream's lifecycle after the append. A peer creates or updates the stream with it, so a stream first seen through replication, and its marker, are retained as long as on the origin rather than under the default ten-minute window; an event without it keeps the peer's current lifecycle (the default for a new stream). Shared Postgres has one metadata row per stream that every server writes, so it needs no lifecycle in its wake hints. The closed state lasts as long as the stream's retained history: once retention deletes the marker the stream is open again, and an idle released stream starts fresh.

Readers receive every message before the marker, then the terminal signal, then the server ends the connection. SSE sends `id: <marker cursor>\nevent: tako.close\ndata: {}\n\n` and ends the response. WebSocket sends a close frame with code 4200 and reason `stream-closed` (`ChannelCloseCode::StreamClosed`, errkit code `SVUM3R`); a WebSocket client publish that races the close gets the same frame. A reader whose cursor is at or after the marker, or that connects at the head of a closed stream, receives the terminal signal at once; one connecting with `from=start` or an earlier cursor replays the retained messages first. The terminal signal is final: clients must not reconnect. `tako.reset`, close code 4410, a 1000 `connection expired` close, and an ended SSE response without `tako.close` remain reconnectable.

The JavaScript SDK's `channel(params).close()` sends `channel_close` with params normalized as for `channel_publish`, `build` when `TAKO_BUILD` is set, and the definition's lifecycle; params and build are omitted when empty or unset, as for publish. It rejects ephemeral definitions and runs only in the server runtime. Its `from: "start"` subscribe/connect option sends `from=start` only when no cursor is given. The SDK treats SSE `tako.close` and WebSocket 4200 as final, calling `close()` on a custom `EventSource`, and never delivers the marker as an application message. JavaScript `defineChannel` and Go `Channels.Register` reject channel and presence params schemas that declare `from` or `last_message_id`; Store params travel as JSON and are not affected. Go has no server-side channel publish or close, network `publish` has no close operation, and neither Go nor Rust has a channel subscribe client.

## Presence

JavaScript, Go, and Rust presence definitions use the channel registry and authorization endpoint, with wire names prefixed by `tako.presence:`. Channel and presence names therefore have separate namespaces. The connect authorization result supplies the trusted `subject`; it must succeed before the server creates a presence session or returns membership.

Presence uses the channel WebSocket route. After authorization, the server sends `{ "type": "tako.presence.ready" }`. The client then sends `{ "type": "tako.presence", "data": ... }` to join or replace its metadata. Client updates cannot choose a subject or connection ID. Metadata is JSON and is limited to 16 KiB.

Server snapshots are `{ "type": "tako.presence", "connectionId": "...", "members": [{ "connectionId": "...", "subject": "...", "data": ... }] }`; anonymous entries omit `subject`. These control frames have no replay ID and are never stored as channel messages. The React hook waits for `ready` before joining so its update cannot precede a browser's first-frame auth envelope.

Membership is scoped by deployed app/environment, wire name, and canonical validated params. Every authorized connection within one scope sees the same members. Params are serialized with recursively sorted object keys, then the wire name and params tuple is hashed with SHA-256 to produce a fixed-size storage key; query ordering does not create separate spaces. Each connection gets a server-generated ID, including separate tabs for one subject.

The client renews a 45-second lease with an update every 15 seconds. Disconnect cleanup removes its record; expiry covers network loss and process crashes. Snapshots poll shared state at approximately one-second intervals. SQLite and Postgres store leases separately from replay, with Postgres keyed by app ID for cross-server visibility. Local dev uses the same WebSocket session loop and an in-memory store, and authorizes connections through the app as production does.

## Stores

JavaScript, Go, and Rust store definitions reuse the channel registry and
authorization endpoint with `tako.store:` wire names. Public reads use
`GET /_tako/stores/<name>?params=<JSON-object>`; live subscriptions use the SSE
channel route with that prefixed name and the same `params` JSON query envelope.
The client URL-encodes the complete object, preserving arrays, objects, nulls,
and numeric/string distinctions. Store channels reject WebSocket/client publishing.
Both routes validate the decoded params object against the registered schema and independently
authorize the subscribe operation. Names use
`^[A-Za-z0-9][A-Za-z0-9._-]{0,255}$`; validated params are JSON objects capped at
16 KiB.

After authorization, the proxy calls `POST /stores/load` on the app's internal
host using the bootstrap token. The request is `{ "store": "<public-name>",
"params": {...}, "subject"?: "<verified-subject>" }`; the subject comes only
from authorization. The SDK responds with `{ "data": ..., "offline"?: {
"maxAgeMs": <positive-safe-integer>, "version": "<1–128-byte-string>" } }`.
Bodies and responses are capped at 1 MiB. Public responses are `no-store`; the
proxy never forwards the bootstrap token or private loader error details.
Loader data is sent only to its requesting reader.

The trusted internal `channel_publish` command publishes an invalidation to the
base `tako.store:<name>` with payload `{ "type": "tako.store.invalidate",
"data": {}, "scopeParams": {...} }`. The native transport derives an opaque
storage key from the wire name and canonical params using SHA-256; it normalizes
equivalent integral numeric values before hashing. Params are consumed as routing
metadata and never sent to subscribers. App/environment identity remains part of
the underlying channel store namespace. Publication affects exactly one params
scope; there is no wildcard invalidation.

Every SSE connection starts from the current notification cursor and sends
`{ "type": "tako.store.ready", "data": {} }` after its change watcher and
cursor are established. Subsequent notifications are
`{ "type": "tako.store.invalidate", "data": {} }`. Store frames carry no
client replay cursor. Clients reload after ready and invalidation, coalesce
notifications during loads, and reload again after reconnect; no snapshot or
intermediate database-state replay is promised.

Development uses the same Store validation and authorization as production,
with in-memory notification storage. Production uses existing channel storage;
cross-server notifications require shared Postgres. Store invalidation is
separate from the application database transaction. Durable commit-and-notify
requires an application-owned outbox.

Offline caching is a client concern gated by the response's opt-in policy and
an explicit client cache scope/version. The browser stores no credentials,
bounds persisted data, honors expiry, and purges the active result and cache
entry on authorization denial. Offline writes and conflict resolution are not
part of this protocol.

## Changing the protocol

Update the typed owner, every producer and consumer, and the serialization or integration tests in one change. Update this document only when a cross-component boundary or invariant changes. User-facing behavior belongs in the website docs and SDK READMEs.

The internal-socket commands the SDKs send, the replies they parse, and the bootstrap envelope have JSON fixtures in `test-support/wire/`. `tako-core` tests generate them from the typed owners; the JavaScript, Go, and Rust SDK tests check their request builders and reply parsers against the same files.

## Public development tunnels

The public tunnel relay (`tako-sh/tunnel`) and `tako-dev-server` negotiate
`tako-tunnel-v2` as their WebSocket subprotocol. Each binary WebSocket message is a
named MessagePack value; byte payloads use MessagePack binary values. The mirrored
`tunnel_wire.rs` files own this contract and must change together. The relay
rejects clients without the subprotocol with HTTP 426. There is no legacy fallback.

HTTP exchanges are `Request` → `Response` headers → `ResponseChunk`* →
`ResponseEnd`. `ResponseError` means the body was truncated; a dropped connection
also fails unfinished bodies. `Cancel` aborts the request task. Each response has
a 128 KiB byte credit window. Each chunk consumes `max(length, 1024)` credits to
bound tiny-frame overhead; the relay sends `ResponseCredit { id, bytes }` for
that amount when its downstream body is consumed. Both peers enforce the budget.
Chunks are at most 64 KiB and are flushed without batching SSE events.
Response headers have a 30-second deadline; body streaming has no total timeout.
Each session allows 32 HTTP requests and 32 WebSockets concurrently. Request
bodies remain buffered with a 2 MiB limit; WebSocket messages are limited to 1 MiB.
The relay admits requests before reading uploads, which must finish within 15
seconds. A relay-wide limit of 128 public HTTP requests and WebSockets also covers
pending uploads and streamed responses; unavailable capacity returns HTTP 503.

Registration challenges are opaque authenticated tokens with a five-minute
expiry. Clients sign the returned nonce unchanged. Only a successfully signed
registration consumes replay-cache space: up to 64 per SSH key fingerprint and
4,096 across the relay until expiry. A key's OpenSSH comment remains part of its
stable URL identity but does not change its quota principal.

WebSocket exchanges start with `WebSocketOpen` → `WebSocketResponse` (HTTP status
and headers). The relay waits up to 30 seconds before upgrading the browser and
uses only the subprotocol selected by the local app. Rejections preserve status
and end-to-end headers with an empty body; local connection failures return 502
and local handshake timeouts return 504. Success precedes all `WebSocketFrame`
messages; `WebSocketClose` cancels both pending handshakes and active sockets.

`Ping { nonce }` / `Pong { nonce }` provides application liveness every 25 seconds
with a 75-second acknowledgement deadline. Reconnects use bounded jitter and only
reset backoff after a stable connection. Session-owned tasks are aborted on every
exit, including cancellation of the session future.

Before merging paired changes, run `python3 scripts/test-conformance.py ../tako`
from the Tunnel checkout. It builds both real peers and checks registration,
HTTP/SSE, WebSockets, cancellation, replacement, and shutdown. Tunnel's CI can
also verify a selected Tako revision using its `tako_ref` workflow input.
