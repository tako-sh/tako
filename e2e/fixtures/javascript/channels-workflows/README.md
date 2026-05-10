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
4. `workflows/broadcast.ts` sleeps briefly then publishes to `demo`.
5. Client receives both messages over the SSE stream without reconnecting.

Used by both the CLI dev e2e suite (`e2e/cli/tests/dev.test.ts`) and the
deploy/docker harness.
