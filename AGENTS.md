# Instructions for AI agents working on Tako

Tako's protocol is v0. Do not preserve legacy behavior, deprecated paths, or backward-compatibility shims; breaking changes are allowed until protocol v1.

## Architecture and scope

- Keep `tako-core` limited to shared protocol types.
- Runtime install commands, launch arguments, and entrypoint paths belong in `tako-runtime/src/plugins/`; preset definitions belong in `presets/{language}.toml`.
- Keep source files focused and below roughly 800 lines. Split by responsibility, and put tests for large modules in a sibling `tests.rs`.
- Validate user input and external APIs, not trusted internal data.
- Do not add abstractions, configurability, or edge-case handling without a concrete need.
- Installable skills in `sdk/javascript/skills/`, `website/public/.well-known/agent-skills/`, `skills/`, and `.agents/skills/` are user-facing. Put maintainer guidance in `AGENTS.md`, repo documentation, or `.agents/internal/`.

## Testing

- Use TDD for Rust crates and SDK code: write the failing test before the implementation. Website changes do not require automated tests.
- Test current behavior, not the absence of removed symbols, flags, messages, fields, or APIs. Negative assertions are appropriate only when they describe a current behavioral distinction.
- Run the affected test suites before finishing. After a config-schema, build/deploy-flow, or protocol refactor, also run `just test cli` and `just test e2e`, updating `e2e/fixtures/` and `e2e/cli/` when necessary.
- Never commit with known test or hook failures unless the user explicitly approves it.

## Documentation

- Code and tests are the source of truth for exact behavior. Website docs own user-facing behavior; focused contract docs own cross-component invariants and architecture.
- Docs7 owns website documentation generation. Keep agent workflow guidance aligned with shipped behavior using `commands/sync-skills.md`.
- Update a component's README when its setup, usage, commands, or test workflow changes.
- When preset definitions change, update the relevant file in `presets/`.
- When SDK exports, adapters, or usage patterns change, update the corresponding user-facing SDK skills. Edit canonical skills in `sdk/javascript/skills/`, then run `bun website/scripts/sync-agent-skills.ts` to refresh repo and website discovery copies. The CLI embeds those skills and `website/docs/` at build time.
- Track planned work in the issue tracker or release notes, not in-repo TODO documents.

## User-facing copy

- Treat CLI output, errors, prompts, documentation examples, and website text as product copy: concise, natural, specific, and action-oriented.
- Before adding or changing `tako` CLI output, read `.agents/internal/cli-output.md`.

## Workflow

- Before changing behavior, inspect the current implementation, tests, and affected documentation.
- Reusable task playbooks live in `commands/`; read the matching playbook before running one.
- Use Conventional Commits: `type(scope): short summary`. Use `chore(repo)` for broad mixed changes.
