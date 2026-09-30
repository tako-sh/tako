# Go workflow worker

This module is imported as `github.com/tako-sh/tako/sdk/go` (package `tako`).
It provides workflow registration, enqueue, signal, and worker APIs. The HTTP
SDK is the separate root module `tako.sh`; use its `ListenAndServe`, `Listener`,
and `GetSecret` helpers for HTTP apps.

Register workflow handlers before calling `RunWorker(ctx)`. The worker reads
`TAKO_WORKER_CONCURRENCY` (default 500) and runs at most that many handlers
concurrently. Cancellation stops new claims and waits for active handlers to
finish; heartbeats and completion RPCs continue during this drain. Handlers must
eventually return for graceful shutdown to finish.

`TAKO_WORKER_IDLE_TIMEOUT_MS` applies only when there are no active handlers.
The idle interval starts again when the last handler finishes.

Run this module's tests from `sdk/go` with `go test ./...`. The concurrency and
drain test also supports `go test -race -run TestWorkerHonorsConcurrencyAndDrains`.

Supply `EnqueueOpts{ID: &id}` to use a caller-defined run ID. Repeating the ID
with the same workflow and payload reuses the retained run in any status;
different inputs conflict. Omit `ID` to generate one. IDs are unique within
the app and environment, must contain 1–255 bytes with no NUL characters,
and remain reserved until history is deleted. Finished history defaults to
184 days; configure `[workflows].retention` or use `"forever"` to change it.
