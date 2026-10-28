# Tako maintainer notes

- Protocol v0 allows breaking changes; update producers and consumers together without compatibility shims. See [PROTOCOL.md](PROTOCOL.md).
- Skills in `sdk/javascript/skills/` are user-facing. Edit them there and run `bun website/scripts/sync-agent-skills.ts` to regenerate `skills/`, `.agents/skills/`, and `website/public/.well-known/agent-skills/`. Keep maintainer guidance out of these skills.
- Maintain website documentation with [commands/sync-docs.md](commands/sync-docs.md). For skills, MCP, and agent discovery changes, follow [commands/sync-agents.md](commands/sync-agents.md). The CLI embeds skills and `website/docs/` at build time.
- Before changing CLI output, read [.agents/internal/cli-output.md](.agents/internal/cli-output.md).
- Config-schema, build/deploy-flow, and protocol refactors require `just test cli` and `just test e2e` in addition to affected tests. Fixtures live in `e2e/cli/` and `e2e/fixtures/`.
