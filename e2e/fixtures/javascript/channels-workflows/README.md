# channels-workflows fixture

Minimal Bun fixture exercising both channels (SSE, via `defineChannel`)
and workflows (enqueue + durable handler, via `defineWorkflow`).

Flow:

1. Client opens `GET /_tako/channels/demo` with `Authorization: Bearer e2e`
   (handled by the Tako dev proxy).
2. Client `POST /publish` with `{ message }` - the fetch handler publishes
   directly to the channel.
3. Client `POST /enqueue` with `{ message }` - the fetch handler enqueues
   the `broadcast` workflow.
4. `src/workflows/broadcast.ts` sleeps briefly then publishes to `demo`.
5. Client receives both messages over the SSE stream without reconnecting.

Used by both the CLI dev e2e suite (`e2e/cli/tests/dev.test.ts`) and the
deploy/docker harness.

`src/presence/lobby.ts` also defines authenticated presence. The CLI dev suite
checks WebSocket joins and disconnect cleanup. Production verifies the `e2e`
token and supplies the `e2e-user` subject; local dev permits anonymous joins.

The `/enqueue` JSON body accepts an optional `id`. Repeated requests with the same ID and message return that run ID while its history is retained.

`src/stores/messages.ts` defines an authenticated Store with nested chat params
and a lazy SQLite backend shared by app instances. `GET /_tako/stores/messages?params={"chat":{"id":"chat-a"}}`
requires `Authorization: Bearer e2e` and returns the trusted subject and current
messages. `POST /store/messages` with that header and `{ "chatId": "chat-a", "message": "hello" }`
commits a row, then invalidates only that chat's Store subscription. The deployment
smoke test checks authorization, subscription readiness, invalidation, and the
refreshed read. Store loaders are never called during type generation.
