# Tako Website

Astro static site deployed with Cloudflare Workers static assets.

## Routes

- `/`: static landing page
- `/docs/`: docs Intro page ("The Why" section first) with docs navigation sidebar (mobile hamburger menu)
- `/docs/quickstart/`: user quickstart (local setup + remote setup)
- `/docs/framework-guides/`: framework adapter examples
- `/docs/cli/`: CLI command reference
- `/docs/tako-toml/`: `tako.toml` configuration reference
- `/docs/development/`: local development guide
- `/docs/deployment/`: deployment guide
- `/docs/troubleshooting/`: troubleshooting runbook
- `/docs/how-tako-works/`: how Tako works overview
- `/docs/channels/`: durable SSE and WebSocket channels
- `/docs/workflows/`: durable workflow authoring and execution guarantees
- `/install.sh`: `301` redirect to GitHub-hosted POSIX `sh` installer script for `tako`
- `/install-server.sh`: `301` redirect to GitHub-hosted POSIX `sh` installer script for `tako-server`
- `/server-install.sh`: alias for `/install-server.sh` (same redirect target)
- `/blog/{slug}/`: HTML blog post
- `/blog/{slug}.md`: authored Markdown for a blog post
- `/blog/{slug}.json`: structured blog post data, including frontmatter, headings, and Markdown
- `/skills.md`: Markdown index of published Agent Skills for agents, including the skills.sh install command

Installer redirects are configured in `public/_redirects` (Cloudflare static assets redirects). Agent-discovery `Link` response headers (RFC 8288) are configured in `public/_headers`.

## Agent Discovery

- `_headers` — RFC 8288 `Link` headers pointing agents at docs, `llms.txt`, `skills.md`, and the sitemap
- `public/.well-known/http-message-signatures-directory` — Web Bot Auth JWKS (Ed25519 public key)
- `public/.well-known/agent-skills/` — Agent Skills Discovery v0.2.0 index + `SKILL.md` copies; regenerated from `sdk/javascript/skills/` by `scripts/sync-agent-skills.ts` on each build
- `/skills.md` — Markdown index that points agents at the structured Agent Skills index, skills.sh install command, and each published `SKILL.md`
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

## Docs7

The guides in `src/pages/docs/` also form a Docs7 project. `docs.json` defines its navigation. Keep pages in Markdown so both Astro and Docs7 can render them. Use paths relative to the docs root for internal links, such as `/quickstart/`. Astro's `remarkDocsLinks` plugin prefixes those paths with `/docs` for the website. Page titles belong in frontmatter; the renderer supplies the heading. `seoTitle` preserves the website's longer HTML title.

Preview Docs7 from the repository root:

```bash
npx @upstash/docs7 dev website/src/pages/docs
```

In the Docs7 dashboard for `tako-sh/tako`, use `master` as the production branch and `website/src/pages/docs` as the docs path. Publishing requires `docs.json` to be on that branch. Connect `tako.sh/docs` through the [Docs7 path proxy setup](https://context7.com/docs/docs7/custom-domains) when switching the public docs.

The Context7 library includes `website/src/pages/docs`, `sdk/javascript`, `sdk/go`, and `sdk/rust`. Exclude `examples` and `sdk/javascript/skills`. Root Markdown files are always included, so exclude maintainer instructions, plans, and internal contracts by filename in the library settings.

## Blog Images

Blog hero sources live in `src/assets/blog/` as PNG files referenced by the `image` frontmatter ID. `bun run build` optimizes those images with Astro and emits compressed OG PNG endpoint files in `dist/assets/blog/og/`.

## Icons

UI icons use the official `@hugeicons/core-free-icons` package. Import icons from their individual package paths and render them with `src/components/HugeIcon.astro`, with `aria-hidden="true"` and `focusable="false"` for decorative icons.
