---
description: Keep Tako documentation in sync with the implementation
---

$ARGUMENTS

# Sync Docs

Synchronize `website/docs/` with shipped behavior. An optional feature, paths, or commit range narrows the scope; otherwise inspect the working tree and relevant branch diff. Work incrementally: preserve accurate material, update affected topics and cross-references, and add missing coverage. Regenerate published output through the build. Do not wipe and rewrite the docs on each run. If nothing affects them, leave them alone.

Read [website/README.md](../website/README.md) for the Astro/Blume architecture and source locations.

1. **Establish the contract.** Read affected implementation, tests, public SDK exports, configuration, presets, and CLI help. Check JavaScript, Go, and Rust separately. The root `tako.sh` Go module is the HTTP SDK; `sdk/go/` is the workflow SDK. State unsupported APIs and runtime limits explicitly; documentation must not imply SDK parity that the code does not provide.
2. **Write for agents first.** Organize by task or capability, with usable commands/APIs, defaults, authorization, persistence, language/container boundaries, and a concrete verification step. Prefer finite commands and structured output. Keep Framework guides and Docker as dedicated sections; put built-in capabilities such as secrets, storage, channels, and workflows under Primitives. Update the shared navigation in `website/src/config/docs-navigation.ts`; avoid redundant group/page labels. Keep exact CLI/config details in references and SDK depth in canonical skills.
3. **Use real language tabs.** Put supported JavaScript, Go, and Rust equivalents in MDX `CodeGroup` components with named fences, such as `ts JavaScript`, `go Go`, and `rust Rust`. Check each example against its SDK and identify app-owned helpers or required setup. Include language-specific Dockerfiles where applicable. Shared browser/React code and language-specific framework guides do not need artificial backend-language tabs. For missing SDK functionality, explain the gap rather than adding placeholder tabs or invented APIs.
4. **Preserve source and discovery contracts.** Keep sources flat in `website/docs/`, using `.md` or `.mdx` for components, single-line `title`/`description` frontmatter, and no duplicate body H1. Keep code readable in the CLI's embedded source; exported Markdown and JSON must retain every language variant. Preserve public URLs and anchors; use docs-root links and add redirects for retired routes. Sidebar and agent indexes derive from shared navigation/frontmatter. For skills, MCP, or discovery changes, follow [sync-agents.md](sync-agents.md); edit canonical skills in `sdk/javascript/skills/` and regenerate their copies.
5. **Keep the site coherent.** Preserve the fixed dark theme, shared code palette, outer gutter, and logo alignment. Browser and Apple icons use the main site's shared artwork via `website/documentation/public/`; OG cards use its logo, palette, and font. Keep page titles, descriptions, canonical URLs, OG/Twitter metadata, and structured data accurate. Astro and Blume compose one static deployment: preserve Context7 Ask, root robots/RSS ownership, and generated agent endpoints. Change sources and build configuration instead of hand-editing generated HTML, metadata, or discovery files.
6. **Verify the result.** Run the checks below and compile or exercise changed examples where practical. When rendering, tabs, navigation, or branding changes, inspect the combined site in desktop/mobile browsers and test relevant interactions. Review the diff and leave unrelated generated artifacts out. Report the topics changed, checks performed, and any real SDK or verification gaps.

```bash
just check website
bun run --filter website verify
git diff --check
```

The verifier checks rendered pages, every code-tab group, Markdown variants, shared icons, canonical/social metadata and image dimensions, API coverage, and agent discovery. Run affected implementation tests when code changes, following `AGENTS.md`. Do not commit, push, or deploy unless requested.
