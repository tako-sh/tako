# Instructions for AI agents working on the Tako codebase

Tako's protocol is v0: do not keep any legacy code, backward compatibility shims, or deprecated paths — break things freely until protocol v1.

## Key Principles

1. **Keep SPEC.md in sync with code** - Whenever you modify code, update SPEC.md if needed (avoid insignificant implementation details; focus on user-facing behavior and architecture)

2. **Move finalized behavior to SPEC.md** - Keep SPEC.md as the source of truth for implemented behavior; keep planning out of in-repo TODO files

3. **TDD is mandatory for Rust crates and SDK code** - Write tests before implementation. No backend/SDK feature is complete without passing tests. Website (`website/`) changes are explicitly exempt.

4. **Don't over-engineer** - Simple is better than complex. Avoid premature abstractions, extra configurability, or handling scenarios that can't happen

5. **Trust internal code** - Only validate at system boundaries (user input, external APIs). Don't add defensive checks between trusted internal components

6. **Keep README files in sync for touched components** - When code changes setup, commands, or run/test flow, update the relevant README.md files. README content should stay basic and practical (what it is, how to run), not a specification.

7. **Keep SPEC-derived website docs in sync** - The following docs are agent-maintained outputs derived from `SPEC.md` and must be updated whenever `SPEC.md` behavior changes:
   - `website/src/pages/docs/how-tako-works.md`
   - `website/src/pages/docs/tako-toml.md`
   - `website/src/pages/docs/presets.md`
   - `website/src/pages/docs/troubleshooting.md`
   - `website/src/pages/docs/cli.md`
   - `website/src/pages/docs/deployment.md`
   - `website/src/pages/docs/development.md`

8. **Runtime behavior lives in plugins** - Runtime definitions (install commands, launch args, entrypoint paths) are in `tako-runtime/src/plugins/`. Preset definitions live in `presets/`.
   - `presets/{language}.toml` — family preset definitions (sections per preset)

9. **Never commit with known failures** - Do not commit when tests or pre-commit hooks are known to be failing. Fix the issues first. If fixing is impractical, get explicit user confirmation before committing.

10. **Keep files small and focused** - No single source file should grow beyond ~800 lines. When adding code to a file that's already large, split it first. But don't split by line count alone — split by responsibility. Ask "is this the right boundary?" not just "is this file too big?" Each module should have one clear responsibility. Prefer sibling submodules (e.g. `commands/init/scaffold.rs`) over letting a file accumulate unrelated concerns. Tests belong in their own `tests.rs` submodule, not inline in large files.

11. **Don't test against removed code or features** - Never write assertions that check a removed symbol, field, flag, message, or API is absent (e.g. `assert!(!output.contains("OLD_NAME"))`, `expect(...).not.toContain("deprecated-flag")`). Once the feature is gone from the code, it cannot reappear, and the test becomes dead weight that rots over time. Negative assertions are only valid when they encode a _current_ behavioral distinction (e.g. "in CI mode, ANSI colors are suppressed"; "NODE_ENV appears in ProcessEnv but not in TakoBaseEnv"). If you catch yourself adding `!contains(...)` for something that no longer exists anywhere in the codebase, delete the assertion instead.

12. **Write polished user-facing copy on the first pass** - Treat every CLI prompt, success message, warning, error, docs example, and website UI string as product copy. Before implementing or finalizing user-facing text, read the whole flow as the user will see it and tighten it. Copy should be short, natural, specific, and action-oriented. Avoid redundant nouns, repeated context, implementation terms, and stiff phrases when the UI already supplies the object, environment, target, or state. Prefer familiar verbs users would say themselves (`Set`, `Updated`, `Replace`, `Remove`, `Try again`) over internal or abstract wording (`configured`, `environment`, `override`, `operation`, `proceed`). Good copy should usually answer: what happened, what needs attention, or what action is next, with no extra explanation unless it prevents a real mistake.

13. **Use internal CLI output guidance** - Before writing or modifying `tako` CLI output, read `.agents/internal/cli-output.md`. This internal guide is not an installable public skill.

## Project Structure

**Rust Crates:**

- `tako-core/` - Shared protocol types (Command, Response enums)
- `tako-runtime/` - Runtime plugins, download engine, caching
- `tako-server/` - Remote server runtime (proxy, instances, TLS, sockets)
- `tako/` - CLI tool (all commands)

**Registry:**

- `presets/` - Preset definitions (e.g. tanstack-start)

**SDKs (current implementation):**

- `sdk/javascript/` - `tako.sh` JavaScript/TypeScript SDK package (npm)
- `sdk/go/` - `tako.sh` Go module
- `sdk/rust/` - `tako` Rust crate

**Website:**

- `website/` - Marketing/docs site (do not add automated tests for this component)

## Build & Test Commands

```bash
# Build all
cargo build

# Build release
cargo build --release

# Test all crates
cargo test

# Test specific crate
cargo test -p tako-cli
cargo test -p tako-runtime
cargo test -p tako-server
cargo test -p tako

# SDK (JS/TS implementation)
cd sdk/javascript && bun install
bun run build && bun run typecheck
bun test

# Rust SDK
cargo test -p tako
```

## Commit Messages

Use Conventional Commits for all commits:

- Format: `type(scope): short summary`
- Common types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`, `ci`
- If scope is broad/mixed across the repo, use `chore(repo): ...`

## Code References

When referring to code, use format: `file_path:line_number`

Example: "Parse app name in `tako/src/app/name.rs:42`"

## Architecture Overview

### Data Flow

1. Developer: `tako deploy` → build locally → SFTP to server
2. Server: Unpack to `/opt/tako/{app}/releases/{version}/`
3. tako-server: Rolling update via unix socket protocol
4. Proxy: Pingora routes to healthy instances

### Key Components

**tako-core:** Minimal, protocol-only. Don't add features here.

**tako-server:**

- `proxy/` - Pingora HTTP/HTTPS proxy
- `instances/` - App lifecycle management
- `lb/` - Per-app load balancer
- `tls/` - Certificate management (ACME)
- `socket.rs` - Unix socket API

**tako CLI:**

- `commands/` - All CLI commands (init, dev, deploy, server, secret, status, logs)
- `config/` - Configuration parsing and merging
- `ssh/` - Remote server communication
- `runtime/` - Runtime detection (Bun, Node)

**SDK:**

- Runtime-agnostic fetch handler interface
- Built-in status endpoint (`Host: <app>.tako`, `/status`) for health checks
- Optional config reload support

## When Making Changes

### Before Writing Code

1. Read existing implementation if it exists
2. Check SPEC.md for expected behavior
3. Confirm scope from the current issue/release context
4. Write tests first (TDD) for Rust crates and SDK changes. Skip test creation for `website/` changes.

### After Writing Code

1. Ensure all applicable tests pass (Rust crates and SDK; website has no test requirement)
2. **After major refactors (config schema changes, build/deploy flow rewrites, protocol changes):** run CLI tests (`just test cli`) and E2E tests (`just test e2e`). Update E2E fixtures (`e2e/fixtures/`) and CLI test cases (`e2e/cli/`) when command behavior or config shape changes.
3. Update SPEC.md if user-facing behavior changed
4. If `SPEC.md` changed, regenerate/sync SPEC-derived website docs:
   - `website/src/pages/docs/how-tako-works.md`
   - `website/src/pages/docs/tako-toml.md`
   - `website/src/pages/docs/presets.md`
   - `website/src/pages/docs/troubleshooting.md`
   - `website/src/pages/docs/cli.md`
   - `website/src/pages/docs/deployment.md`
   - `website/src/pages/docs/development.md`
5. If preset definitions changed, update the relevant `presets/<language>.toml` file.
6. If SDK exports, adapters, or usage patterns changed, update the agent skills in `sdk/javascript/skills/` to match.
7. Update affected README.md files if setup/usage/run commands changed
8. Close or update the related issue/task entry
9. Keep implementation details OUT of SPEC.md (focus on what users see/do)

### Example Changes

**If fixing a bug (Rust/SDK):** Add test that reproduces the bug, then fix code. Update SPEC.md only if it reveals a spec gap.

**If fixing a website bug:** Implement the fix directly without adding tests.

**If adding a feature (Rust/SDK):** Confirm scope first, write tests first, implement code, update SPEC.md, then close/update the related issue/task.

**If adding a website feature:** Confirm scope first, implement code without adding tests, update docs/spec as needed, then close/update the related issue/task.

**If changing architecture:** Confirm and record scope first, implement with TDD, then document the result in SPEC.md and close/update the related issue/task.

## Common Tasks

### Adding a new CLI command

1. Add command variant to `tako/src/cli.rs` (clap parser)
2. Create `tako/src/commands/{command}.rs` module
3. Implement command function
4. Write integration tests in `tako/tests/`
5. Update SPEC.md with command description and examples

### Adding a new message type to protocol

1. Add variant to `tako_core::Command` or `tako_core::Response` enum
2. Add handler in `tako_server/src/socket.rs`
3. Add sender in `tako/src/commands/` or SDK as needed
4. Write tests in `tako-core/src/lib.rs`
5. Update SPEC.md "Communication Protocol" section

### Fixing a deployment bug

1. Identify the issue (edge case in SPEC.md?)
2. Add test that reproduces the bug
3. Fix code in `tako/src/commands/deploy.rs` or `tako-server/`
4. Verify fix with test
5. Update SPEC.md if behavior changed or was clarified

## Testing Patterns

Rust crates and SDK code should be tested following TDD. Do not add tests for `website/` changes.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_happy_path() {
        // Test normal operation
    }

    #[test]
    fn test_error_case() {
        // Test error handling
    }

    #[test]
    fn test_edge_case() {
        // Test boundary conditions
    }
}
```

Integration tests in `{crate}/tests/` directory for command-level behavior.

## Documentation Standards

- SPEC.md: High-level, user-focused, finalized behavior with examples
- SPEC-derived website docs (auto-maintained by agent): keep these aligned with SPEC.md after every behavior change:
  - `website/src/pages/docs/how-tako-works.md`
  - `website/src/pages/docs/tako-toml.md`
  - `website/src/pages/docs/presets.md`
  - `website/src/pages/docs/troubleshooting.md`
  - `website/src/pages/docs/cli.md`
  - `website/src/pages/docs/deployment.md`
  - `website/src/pages/docs/development.md`
- README.md files: Basic orientation and practical usage (`what this is`, `how to run/test`). Keep concise and non-normative.
- Planning/issues: Track planned work in the issue tracker or release notes; do not add in-repo TODO files
- Code comments: Only where logic isn't self-evident
- Test names: Describe what is being tested (not "test1", "test2")

## Tech Stack Reference

- **Proxy:** Pingora (Cloudflare's library)
- **Async:** Tokio
- **TLS:** OpenSSL callbacks (Pingora) + RCGen (self-signed dev certs)
- **ACME:** instant-acme (Let's Encrypt)
- **SSH:** Russh
- **CLI:** Clap 4.5
- **Config:** TOML 0.8
- **Serialization:** Serde 1.0

## Performance Targets

- Proxy: low overhead, should not be the bottleneck on the request path
- Cold start: ~100-500ms
- Health detection: <3s
- Deploy time: <1 min per rolling update
- Memory: Minimal with on-demand scaling

## Documentation Workflow

```
Scope agreement → Code (TDD) → SPEC.md + README.md updates → Cleanup
```

1. **Scope agreement** - Confirm issue/task and acceptance criteria
2. **Code changes** - Implement with tests for Rust/SDK changes (TDD required there). Do not create tests for `website/` changes.
3. **SPEC.md** - Document finalized behavior after implementation
4. **README.md** - Update affected component readmes for basic usage/run instructions
5. **Cleanup** - Close/update related issue or release task

## Available Commands

Reusable agent commands live in `commands/`. Read the referenced file for full instructions before executing.

- **`commands/sweep.md`** — Security, performance, and code quality audit. Fixes trivial issues, reports the rest.
- **`commands/performance.md`** — Run and publish Tako proxy performance benchmarks, update the performance repo report, and keep the main repo summary short.
- **`commands/spec-sync.md`** — Reconcile SPEC.md with code, then regenerate all SPEC-derived website docs.
- **`commands/code-scanning-resolve.md`** — Resolve all open GitHub code scanning alerts (fix, dismiss, or ask for input).
- **`commands/blog-post.md`** — Create a new blog post from a topic/idea. Researches context, writes the post, verifies build.
