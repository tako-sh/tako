---
description: Reconcile Tako agent skills with shipped behavior and refresh their distribution copies
---

# Sync Skills

Update the skills that teach coding agents how to develop, debug, deploy, and verify Tako applications. Docs7 owns website documentation generation; use its guides as context and code and tests for exact behavior.

## Sources of truth

- CLI arguments and defaults: `tako/src/cli.rs`, command implementations, and CLI tests.
- MCP tools, schemas, annotations, and execution: `tako/src/commands/mcp/` and its tests.
- Agent installation and project instructions: `tako/src/commands/agents/` and CLI integration tests.
- SDK APIs and examples: public exports and tests in `sdk/javascript/`, `sdk/go/`, and `sdk/rust/`.
- Configuration and runtime behavior: parsers, validation, runtime plugins, presets, and their tests.
- User workflows and architecture: the relevant Docs7 source pages in `website/docs/`.

If documentation disagrees with shipped behavior, ground the skill in code and tests and report the discrepancy. Do not describe planned or unshipped APIs as available.

## Update canonical skills

Edit `sdk/javascript/skills/`:

- `tako`: project context, local development, debugging, deployment, verification, and operational decisions.
- `tako-sdk-js`, `tako-sdk-go`, and `tako-sdk-rust`: language-specific APIs, framework integration, and working examples.

For a scoped change, inspect only the affected skills and their sources. For a full refresh, inspect all four skills.

Check that each affected skill:

1. Has a clear description that helps an agent recognize when to use it.
2. Explains the workflow, the evidence to inspect, and how to verify the outcome.
3. Uses current public SDK APIs and executable examples.
4. Refers to existing MCP tools and documentation topics, with accurate environment and project scope.
5. Describes consequential operations and secret handling accurately.
6. Keeps command reference material in CLI help, MCP schemas, and docs where possible.

Keep maintainer instructions in `AGENTS.md`, repository documentation, or `.agents/internal/`. Installable skills are for agents working on users' Tako applications.

MCP schemas are generated from CLI definitions. If a capability, annotation, or execution behavior needs changing, update its implementation and tests; prose alone does not add a tool.

## Refresh distribution copies

Run from the repository root:

```bash
bun website/scripts/sync-agent-skills.ts
```

This updates `skills/`, `.agents/skills/`, the website's `.well-known/agent-skills/` files, and its discovery index. Edit the canonical SDK skills before running this script.

The CLI embeds the canonical skills and `website/docs/` at build time. Installed projects receive refreshed skills when users upgrade Tako and rerun `tako agents`.

## Verify

- Validate changed examples against the relevant SDK or CLI behavior and run the affected tests when code changes.
- Check that distribution copies match their canonical skill and that discovery hashes match the published files.
- Run `git diff --check` and check formatting for the changed Markdown files.
- If CLI or MCP behavior changed, run the affected Rust and CLI integration suites.

Summarize the workflows updated, verification performed, and any code or documentation discrepancies that remain.
