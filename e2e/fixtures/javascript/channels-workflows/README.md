# channels-workflows fixture

Minimal Bun fixture exercising both channels (SSE, via `defineChannel`)
and workflows (enqueue + durable handler, via `defineWorkflow`).

Flow:

1. Client opens `GET /_tako/channels/sse/demo` with `Authorization: Bearer e2e`
   (handled by the Tako dev proxy).
2. Client `POST /publish` with `{ message }` - the fetch handler publishes
   directly to the channel.
3. Client `POST /enqueue` with `{ message }` - the fetch handler enqueues
   the `broadcast` workflow.
4. `src/workflows/broadcast.ts` sleeps briefly then publishes to `demo`.
5. Client receives both messages over the SSE stream without reconnecting.

Run-addressed signals: `POST /approval` with `{ id, settleMs }` enqueues the
`approval` workflow, which sleeps `settleMs` and then waits for `decision`.
`POST /signal` with `{ runId, event, payload }` sent during the sleep must be
`buffered` and later returned by the wait; a second one is a `duplicate`, and
one after the run finished is `finished`. `POST /approval/result` reports the
run's result.

Used by both the CLI dev e2e suite (`e2e/cli/tests/dev.test.ts`) and the
deploy/docker harness.

The deployment harness also installs `caller/` as a second app.
The caller's `schemas` imports the producer's generated `tako-schema.json`; generation writes its `tako.imports.ts`.
`POST /call` invokes `echo`, which directly calls `uppercase`; `POST /run` starts
the caller's `relay` workflow, durably waits for `broadcast`, and returns its
stored result. The harness checks both responses and rejects public access to
the private network dispatch endpoint.

`src/presence/lobby.ts` also defines authenticated presence. The CLI dev suite
checks WebSocket joins and disconnect cleanup. Production verifies the `e2e`
token and supplies the `e2e-user` subject; local dev permits anonymous joins.

The `/enqueue` JSON body accepts an optional `id`. Repeated requests with the same ID and message return that run ID while its history is retained.

`src/stores/messages.ts` defines an authenticated Store with nested chat params,
backed by the `messages` database binding. `tako dev` serves it from a local
file; the deployment harness creates it with `tako db create` on the first
server with network access, so the second server reaches it over the private
network. `GET /_tako/stores/messages?params={"chat":{"id":"chat-a"}}`
requires `Authorization: Bearer e2e` and returns the trusted subject and current
messages. `POST /store/messages` with that header and `{ "chatId": "chat-a", "message": "hello" }`
commits a row, then invalidates only that chat's Store subscription. The deployment
smoke test checks authorization, subscription readiness, invalidation, and the
refreshed read. Store loaders are never called during type generation.

`just e2e e2e/fixtures/javascript/channels-workflows` deploys `cw-e2e/production`
first to Ubuntu, then to AlmaLinux in the same two-member network. The full client
creates `channel-replay` and `workflow-runs` on Ubuntu once (`tako db create <name> --server server-ubuntu`).
`[envs.production.channels].db` and `[envs.production.workflows].db` select them
on both deployments, so both servers share replay and workflow runs. App
instances and release commands see only the `messages` SQL binding. After the
Store export/import checks, both nodes publish to `demo`; subscribers resume
through the other node without a reset. `tako db status` checks the
replay-only binding and peer grant; `tako db shell` matches each returned
cursor and message to its row.
