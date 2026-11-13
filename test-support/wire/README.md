# Internal socket wire fixtures

One JSON file per message on Tako's internal Unix socket, as the server reads
and writes it:

- `commands/<command>[-<variant>].json`: a command an SDK sends, named by its
  `command` field.
- `replies/<name>.json`: a reply an SDK parses. `error-<code>.json` exists for
  every socket error code; `error.json` has no code and
  `error-unknown_code.json` has a code no SDK knows.
- `bootstrap.json`: the envelope a process reads from fd 3 or
  `TAKO_BOOTSTRAP_DATA`.

The files describe the server's contract. `tako-core` owns them:
`tako-core/src/wire_fixtures.rs` builds each message from the typed protocol
values (`tako_core::Command`, `tako_core::Response`, the bootstrap envelope),
asserts that it serializes to the checked-in file, and that the file
deserializes back to the same value. It also fails when a file is missing or
no longer generated.

The SDK tests load the same files and check that their real request builders
send exactly these commands and that their reply parsers accept these replies:

- JavaScript: `sdk/javascript/tests/wire-fixtures.test.ts`
- Go: `wire_fixtures_test.go` and `internal/wire_fixtures_test.go` (root
  module), `sdk/go/wire_fixtures_test.go`
- Rust: `sdk/rust/src/wire_fixtures.rs`

Comparisons use parsed JSON, so formatting does not matter. Values are fixed
(app `demo`, run `run-1`, claim `claim-7`, timestamps around
2026-01-01T00:00:00Z); the SDK tests pass the same values to the SDK.

## Changing a message

Change the typed owner in `tako-core`, then regenerate the files:

```sh
TAKO_UPDATE_WIRE_FIXTURES=1 cargo test -p tako-core --lib wire_fixtures
```

Run `bun run fmt` on the files, then every SDK's tests, and update each SDK
whose test fails. One file per message keeps each change to the contract
visible in its own diff.

## SDK decisions

- A nil/undefined enqueue payload is JSON `null`, never an empty object.
- Store invalidations omit lifecycle settings, preserving the stream settings
  established by subscribers. Every SDK sends `build` when `TAKO_BUILD` is set,
  allowing the server to validate against that release's cached registry.
- Unknown error codes are unclassified in every SDK. Go retains the raw text
  in `ServerError.Code`; JS `reason` and Rust `ErrorCode` expose only known values.
  The shared unknown-code reply pins this intentional API difference.
- JS `waitFor` uses an omitted timeout for indefinite waiting and explicit zero
  for immediate expiry. Go's required duration argument uses zero for indefinite
  waiting. Step tests pin these language-specific calling conventions; both send
  a nullable absolute deadline on the wire.
- Public RPC errors redact server diagnostics. Workflow workers retain the
  server's diagnostic in the stored run error, including unclassified rejections.
