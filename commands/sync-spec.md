---
description: Reconcile SPEC.md with code, then regenerate docs
---

# Spec Sync — Reconcile SPEC.md with Code, Then Regenerate Docs

## Goal

Bring `SPEC.md` into perfect alignment with the actual codebase, then fully regenerate all SPEC-derived website docs from the updated spec.

This is a two-phase process:

1. **Audit & refine SPEC.md** — find every discrepancy between code and spec (missing features, outdated descriptions, wrong defaults, stale examples, removed behavior still documented, implemented behavior not yet documented).
2. **Regenerate docs** — fully rewrite each SPEC-derived doc page so it coherently reflects the updated spec. Not a patch or append — each page should read as if written fresh from the current spec.

---

## Phase 1: Audit SPEC.md Against Code

### What to read

Systematically read the source code across all components. Do not skip files — cover:

- **tako-core/src/** — protocol types (`Command`, `Response` enums, fields, variants)
- **tako-server/src/** — server runtime (proxy, instances, health, scaling, socket, metrics, TLS, LB, rolling updates, state store)
- **tako/src/** — CLI tool:
  - `cli.rs` — clap parser (all commands, flags, arguments)
  - `commands/` — every command implementation (init, dev, deploy, delete, scale, logs, status, server, secret, releases, upgrade, doctor)
  - `config/` — config parsing, merging, validation
  - `build/` — build system (presets, artifacts, adapters)
  - `validation/` — config validation rules
  - `ssh/` — remote communication
  - `paths.rs`, `output.rs` — paths, output modes
- **sdk/javascript/src/** — SDK (fetch handler, adapters, vite plugin, status endpoint)
- **scripts/** — install scripts (install-tako.sh, install-tako-server.sh)
- **tako-runtime/src/** — runtime registry types, download engine, embedded package-manager metadata
- **presets/** — preset family manifests:
  - `presets/{language}.toml` — preset family definitions
  - `presets/_example.toml` — example preset manifest schema

### What to look for

For each SPEC.md section, compare against the code and flag:

1. **Missing from spec** — behavior implemented in code but not documented in SPEC.md
2. **Stale in spec** — behavior described in SPEC.md that no longer matches code (changed defaults, renamed flags, removed features, different error messages)
3. **Wrong details** — incorrect default values, wrong flag names, wrong file paths, wrong environment variable names
4. **Missing protocol messages** — `Command`/`Response` variants in tako-core that aren't documented
5. **Inconsistent examples** — code examples or config snippets that don't match actual format
6. **Organization issues** — sections that could be better organized, duplicated information, unclear wording

### Output format for Phase 1

Before making changes, produce a concise discrepancy report:

```
## Discrepancies Found

### Missing from SPEC
- [brief description] (source: file:line)

### Stale/Wrong in SPEC
- [what SPEC says] → [what code actually does] (source: file:line)

### Organization/Polish
- [description of improvement]
```

Then apply all fixes to `SPEC.md`. Preserve the existing structure and style. Keep implementation details out — focus on user-facing behavior and architecture. Remove anything that describes planned/unimplemented features unless clearly marked as such.

---

## Phase 2: Regenerate SPEC-Derived Docs

After SPEC.md is updated, regenerate each of these doc pages **from scratch** based on SPEC.md:

- `website/src/pages/docs/how-tako-works.md`
- `website/src/pages/docs/tako-toml.md`
- `website/src/pages/docs/presets.md`
- `website/src/pages/docs/troubleshooting.md`
- `website/src/pages/docs/cli.md`
- `website/src/pages/docs/deployment.md`
- `website/src/pages/docs/development.md`

### Doc generation rules

- **Read each existing doc first** to understand its frontmatter (title, description, layout, etc.) and general scope/audience. Preserve the frontmatter format.
- **Rewrite the body entirely** from SPEC.md — do not copy-paste from SPEC. Transform the spec into user-friendly documentation:
  - Use a natural, approachable tone (not a dry specification listing)
  - Add practical examples and common workflows
  - Organize for discoverability (readers scan, not read linearly)
  - Use clear headings, short paragraphs, code blocks with realistic examples
- **Each doc page has a defined scope** — only include SPEC.md content relevant to that page:
  - `how-tako-works.md` — architecture overview, data flow, key concepts
  - `tako-toml.md` — complete `tako.toml` reference with all options
  - `presets.md` — preset system, built-in presets, custom presets
  - `troubleshooting.md` — common issues, error messages, recovery steps
  - `cli.md` — all CLI commands, flags, usage examples
  - `deployment.md` — deploy workflow, server setup, rolling updates, scaling
  - `development.md` — `tako dev` workflow, local CA, DNS, hot reload
- **Do not invent features** — only document what SPEC.md describes
- **Do not add internal implementation details** — keep docs user-focused

---

## Phase 3: Sync Preset Examples

After updating SPEC.md and docs, verify that `presets/_example.toml` matches the current preset schema:

- `presets/_example.toml` — must include every field and runtime override shape used in real preset TOML files
  Compare it against `presets/javascript.toml` and `presets/go.toml`. If any field/section is missing or outdated, update the example.

---

## Phase 4: Commit

After all phases are complete, create a single commit on the current branch with all changes. Use a conventional commit message like:

```
docs(spec): sync SPEC.md with code and regenerate docs
```

Do not push — just commit locally.

---

## Rules

- Read code thoroughly before changing SPEC.md — evidence over assumptions.
- SPEC.md changes must be grounded in actual code behavior.
- Keep SPEC.md focused on user-facing behavior and architecture, not implementation details.
- Doc pages are full rewrites, not patches — they should read as coherent standalone documents.
- Preserve frontmatter in doc files.
- Do not create new doc files — only update the listed ones.
