# Tako Website

Astro static site deployed with Cloudflare Workers static assets.

## Routes

- `/`: static landing page
- `/docs`: docs Intro page ("The Why" section first) with docs navigation sidebar (mobile hamburger menu)
- `/docs/quickstart`: user quickstart (local setup + remote setup)
- `/docs/framework-guides`: framework adapter examples
- `/docs/cli`: CLI command reference
- `/docs/tako-toml`: `tako.toml` configuration reference
- `/docs/development`: local development guide
- `/docs/deployment`: deployment guide
- `/docs/troubleshooting`: troubleshooting runbook
- `/docs/how-tako-works`: how Tako works overview
- `/install.sh`: `301` redirect to GitHub-hosted POSIX `sh` installer script for `tako`
- `/install-server.sh`: `301` redirect to GitHub-hosted POSIX `sh` installer script for `tako-server`
- `/server-install.sh`: alias for `/install-server.sh` (same redirect target)
- `/blog/{slug}.md`: authored Markdown for a blog post
- `/blog/{slug}.json`: structured blog post data, including frontmatter, headings, and Markdown

Installer redirects are configured in `public/_redirects` (Cloudflare static assets redirects). Agent-discovery `Link` response headers (RFC 8288) are configured in `public/_headers`.

## Agent Discovery

- `_headers` — RFC 8288 `Link` headers pointing agents at docs, `llms.txt`, and the sitemap
- `public/.well-known/http-message-signatures-directory` — Web Bot Auth JWKS (Ed25519 public key)
- `public/.well-known/agent-skills/` — Agent Skills Discovery v0.2.0 index + `SKILL.md` copies; regenerated from `sdk/javascript/skills/` by `scripts/sync-agent-skills.ts` on each build
- Blog posts expose explicit `.md` and `.json` endpoints from `src/pages/blog/`.
- WebMCP tools (`navigator.modelContext.provideContext`) registered in `src/layouts/BaseLayout.astro` — `navigateToDocs`, `searchDocs`, `getStartedCommand`, `getInstallCommand`. Feature-detected, silently no-ops in browsers without the API.

## Run Locally

```bash
bun install
bun run --cwd website dev
```

## Test Installer Endpoints Locally

```bash
curl -fsSL http://localhost:4321/install.sh | sh
curl -fsSL http://localhost:4321/install-server.sh | sudo sh
```

## Build and Deploy

```bash
bun run --cwd website build
bun run --cwd website deploy
```

## Blog Images

Blog hero sources live in `src/assets/blog/` as PNG files referenced by the `image` frontmatter ID. `bun run build` optimizes those images with Astro and emits OG PNG endpoint files in `dist/assets/blog/og/`.
