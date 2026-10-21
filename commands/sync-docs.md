---
description: Audit shipped behavior and synchronize Tako documentation
---

# Sync Docs

Reconcile Tako's documentation with shipped code and tests. Update each documentation surface directly. Do not create an intermediate specification.

## Sources of truth

Use the narrowest executable source for each claim:

- CLI commands and flags: `tako/src/cli.rs`, command implementations, and CLI tests.
- Configuration and defaults: config schemas, parsers, validation, and their tests.
- Runtime behavior: `tako-runtime/src/plugins/`, `presets/*.toml`, server code, and integration tests.
- Protocol shapes: `tako-core/src/`, `tako-socket/src/`, and serialization tests.
- SDK APIs: exported types and functions, SDK tests, and generated declarations.
- Cross-component guarantees: `PROTOCOL.md` plus the code and tests it links to.

Code and tests win when prose disagrees. If the code appears wrong, report it separately instead of documenting intended but unshipped behavior.

## Audit

For a scoped change, read the affected website docs, READMEs, examples, and installable skills. For a repository-wide audit, read every page in `website/docs/` and every other affected documentation surface. Compare each claim, default, command, path, environment variable, and example with its executable source.

For a repository-wide audit, record discrepancies before editing:

```markdown
## Discrepancies

### Missing

- Behavior absent from docs. Source: `path:line`.

### Stale or incorrect

- Existing claim and shipped behavior. Source: `path:line`.

### Organization

- Duplication or misplaced material to simplify.
```

For a repository-wide audit, cover:

1. Architecture, routing, health, TLS, storage, backups, channels, workflows, and observability.
2. Every CLI command and global option.
3. Every `tako.toml` field, merge rule, default, and validation constraint.
4. Runtime plugins and all preset families.
5. JavaScript, Go, and Rust SDK exports and usage patterns.
6. Setup, development, deployment, troubleshooting, and framework guides.

## Update

- Edit affected docs in place. Preserve page frontmatter and keep each page standalone.
- Write for users. Prefer practical examples, short sections, and links to deeper references.
- Keep internal schemas and implementation detail in code unless maintainers need the invariant to change multiple components safely.
- Update affected component READMEs, examples, and installable skills in the same change.
- Verify `presets/_example.toml` against every real preset file when preset behavior changes.
- Update historical blog posts only when a link or present-tense product claim became false.

## Verify

Run:

```bash
just check::website
git diff --check
```

Run relevant Rust or SDK tests when documentation examples depend on behavior not already covered by the website build.

Summarize the corrected claims, verification results, and any product or implementation discrepancies left for follow-up.
