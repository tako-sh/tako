---
description: Synchronize Tako skills, MCP support, and agent discovery files with shipped behavior
---

# Sync Agents

Maintain Tako's complete agent-facing experience: workflow skills, MCP capabilities, project setup, documentation discovery, and generated distribution files. Docs7 owns website documentation generation. Code and tests establish exact shipped behavior.

## Sources and generated artifacts

| Surface                                      | Source of truth                                                                      | Generation                                                                                                |
| -------------------------------------------- | ------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------- |
| Workflow and SDK skills                      | `sdk/javascript/skills/`                                                             | The skill sync script refreshes `skills/`, `.agents/skills/`, and website discovery copies and hashes.    |
| MCP tools and schemas                        | `tako/src/cli.rs` and `tako/src/commands/mcp/`                                       | Tool schemas derive from CLI definitions; dedicated handlers implement docs search and finite local logs. |
| Project agent setup                          | `tako/src/commands/agents/`                                                          | `tako agents` installs bundled skills, MCP settings, and project guidance.                                |
| Root `/llms.txt`                             | Docs7 navigation, page frontmatter, blog collection, and `website/src/utils/llms.ts` | The Astro endpoint generates the index during the website build.                                          |
| `/docs/llms.txt` and `/docs/llms-full.txt`   | Docs7 documentation                                                                  | Docs7 generates these; the website Worker proxies them.                                                   |
| `/skills.md` and `.well-known/agent-skills/` | Canonical SDK skills                                                                 | Website generation publishes descriptions, URLs, contents, and hashes.                                    |
| Bundled MCP docs and skills                  | `website/docs/` and canonical SDK skills                                             | `tako/build.rs` embeds them into each CLI build.                                                          |

For a scoped change, inspect the affected sources and surfaces. For a full refresh, inspect all of them. Update sources first and regenerate their outputs.

## Reconcile skills and capabilities

Read the relevant command implementations, config parsers, validation, runtime plugins, presets, public SDK exports, examples, and tests. Use the Docs7 guides for workflow and architecture context. If prose disagrees with code, ground agent guidance in shipped behavior and report the discrepancy.

Edit canonical skills in `sdk/javascript/skills/`:

- `tako`: project context, local development, debugging, deployment, verification, and operational decisions.
- `tako-sdk-js`, `tako-sdk-go`, and `tako-sdk-rust`: language-specific APIs, framework integration, and working examples.

Skills should explain when to use a capability, what evidence to inspect, and how to verify the result. Keep exact command arguments in CLI help, MCP schemas, and reference docs where possible. Keep maintainer instructions in `AGENTS.md`, repository documentation, or `.agents/internal/`.

Check that MCP tools cover the affected workflow, use accurate read/write annotations, honor the selected project/environment, and handle stdin, finite execution, output limits, and cancellation correctly. Change implementations and their tests when capabilities change; editing prose does not add a tool. Verify relevant client setup when changing installation or discovery behavior.

## Keep discovery current

The root `llms.txt` reads `website/docs/docs.json` navigation and the corresponding Markdown titles/descriptions. Pages appear in Docs7 navigation order. The blog collection supplies newest-first Markdown links under Optional. Product guidance and project links live in `website/src/utils/llms.ts`.

When Docs7 adds or renames a page, ensure its navigation entry points to a source page with title frontmatter, then rebuild the website. Keep the root index concise and link to Docs7's generated full-documentation endpoint for complete text. Avoid copying full docs into skills or creating another manually maintained page list.

Check that skills point to existing tools and documentation topics, that `/skills.md` and the discovery index agree, and that Context7 search information remains available in generated responses and discovery headers.

## Regenerate and verify

Run from the repository root:

```bash
bun website/scripts/sync-agent-skills.ts
bun run --filter website check
bun run --filter website build
bun run --filter website verify
git diff --check
```

The website build also runs the skill sync script. Verification checks generated HTML/Markdown discovery information, Docs7 page and blog coverage in `llms.txt`, and published skill contents and hashes. CI runs verification after website or canonical skill changes and checks that regeneration leaves tracked skill distribution files unchanged.

Check formatting on changed files and validate changed examples against the affected SDK or CLI behavior. Run the relevant Rust, SDK, and CLI integration suites when their implementations change. Follow `AGENTS.md` for additional checks required by configuration, build/deploy, or protocol changes.

Review generated changes before committing; leave unrelated build artifacts out. Website deployment publishes discovery files, Docs7 publishes its documentation endpoints, and a new CLI build carries updated embedded docs/skills. Existing projects refresh installed skills and MCP settings by upgrading Tako and rerunning `tako agents`.

Summarize the surfaces updated, verification performed, and any remaining code or documentation discrepancies.
