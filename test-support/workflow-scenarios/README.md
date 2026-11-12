# Workflow worker scenarios

Each JSON file here scripts one conversation between a workflow worker and a
fake internal socket. The JavaScript and Go workers both play every file, with
the same expectations, so the two cannot drift apart on the socket contract
(see `PROTOCOL.md` and `tako-workflows/src/`).

| SDK        | Runner                                             | Runs in                         |
| ---------- | -------------------------------------------------- | ------------------------------- |
| JavaScript | `sdk/javascript/tests/workflows/scenarios.test.ts` | `bun test --cwd sdk/javascript` |
| Go         | `sdk/go/scenario_test.go`                          | `cd sdk/go && go test ./...`    |

The Rust SDK has no workflow worker, so it has no runner.

## Format

```json
{
  "description": "What the scenario proves.",
  "drive": "run",
  "workflows": ["steps"],
  "steps": [
    { "expect": { "command": "claim_run", "...": "..." }, "reply": { "status": "ok", "data": {} } }
  ]
}
```

- `workflows` names the test workflows (below) the worker registers. The app
  name is always `scenario-app`.
- `steps` is the whole conversation, in order. For each request the worker
  sends, the runner matches it against the next step's `expect` and answers
  with that step's `reply`, sent verbatim.
- `drive` says how the runner exercises the worker:
  - `run`: the worker claims once and executes the run it claimed, as its
    claim loop does for each run (JavaScript `Worker.processOnce()`; Go the
    worker's `Claim` call followed by `execute`). The first step is the
    `claim_run`.
  - `start`: the worker starts the way its process entrypoint does
    (JavaScript `bootstrapWorker` discovering the named workflows; Go
    `RunWorker` after `RegisterWorkflow`) and is stopped before its claim loop
    begins. The steps cover what the worker sends before its first claim.

A runner fails a scenario when a request does not match, when the worker sends
a request after the last step, when the worker finishes before the last step,
and when the file has a field, drive, workflow, or matcher the runner does not
know. Nothing is skipped.

Heartbeats cannot interleave: runners set the heartbeat interval to one hour,
so a `heartbeat_run` request is unexpected like any other.

## Matching rules

An `expect` value matches a request value as follows.

- A string, number, boolean, or `null` matches an equal value. Numbers compare
  by value.
- An array matches an array of the same length whose elements match in order.
- An object without `$` keys matches an object with exactly the same keys
  whose values match. A missing key and an extra key both fail; a key whose
  value is `null` must be present.
- An object with a `$` key is a matcher:

| Matcher                        | Matches                                                                                                                          |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| `{"$any": true}`               | Any value, including `null`.                                                                                                     |
| `{"$type": "string"}`          | A value of that JSON type: `string`, `number`, `boolean`, `object`, `array`, or `null`.                                          |
| `{"$positive": true}`          | A number greater than zero.                                                                                                      |
| `{"$contains": "text"}`        | A string containing `text`.                                                                                                      |
| `{"$ms_from_now": [min, max]}` | A Unix-millisecond timestamp `t` with `min <= t - now <= max`, where `now` is the runner's clock when the request arrived.       |
| `{"$var": "name"}`             | On first sight in the scenario, any non-null value, which is bound to `name`. Later, only a value deep-equal to the bound value. |
| `{"$var": "name", "is": M}`    | As `$var`, but on first sight the value must also match `M`.                                                                     |
| `{"$unordered": [M1, M2]}`     | An array of the same length whose elements match the listed matchers in some order.                                              |

Variables bind generated and clock-derived values: the worker's process id,
the lease it asks for, and wake times that a later request must repeat.
Durations are checked through `$ms_from_now` windows. Retry times use the
default run-level backoff, one second with 20% jitter, so the first retry
lands 800 to 1200 ms out; the window's lower bound is looser to absorb
request latency.

Free-text error messages are not contract. Scenarios compare them only with
`$contains` or `$type`.

## Test workflows

Both runners register these, with the same behavior. Payloads are ignored.

| Name           | Behavior                                                                                                   |
| -------------- | ---------------------------------------------------------------------------------------------------------- |
| `steps`        | Step `a` returns `"a-done"`, step `b` returns `2`; the run's result is `{"a": <a>, "b": <b>}`.             |
| `fails`        | Fails with the error `boom` (JavaScript throws `new Error("boom")`; Go returns `errors.New("boom")`).      |
| `sleeps`       | Sleeps one hour in step `nap`, then returns `"awake"`.                                                     |
| `wait`         | Waits for the event `approval` without a timeout and returns its payload.                                  |
| `wait-timeout` | Waits for the event `approval` with a ten-minute timeout and returns its payload.                          |
| `duplicate`    | Step `a` returns `1`, then a second step named `a` returns `2`.                                            |
| `bail`         | Bails with the reason `not needed`.                                                                        |
| `nightly`      | Scheduled `0 3 * * *` with five attempts (JavaScript `retries: 4`; Go `WithMaxAttempts(5)`); does nothing. |
| `hourly`       | Scheduled `0 * * * *` with the server's default budget; does nothing.                                      |

The JavaScript definitions are in `sdk/javascript/tests/workflows/scenario-workflows.ts`
and the Go ones in `sdk/go/scenario_workflows_test.go`.
