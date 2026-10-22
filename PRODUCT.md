# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Register

brand

## Users

Tako has two primary audiences, and both carry equal weight:

- **Developers and small teams** who want to run their own apps on their own servers without turning deployment into a separate operations project. They move between local development, server setup, docs, CLI output, and production deploys, so the public site must help them understand the tool quickly and trust the details.
- **The coding agents those developers delegate to** (Codex, Claude, and others). An agent may read the site, docs, `llms.txt`, and `SKILLS.md`, then migrate a project ("Move this project to tako.sh") or wire a feature in. Pages must work when an agent is the reader.

## Product Purpose

Tako makes self-hosted app deployment feel direct: install the CLI, run local HTTPS development, connect a server, and deploy with rolling updates, TLS, secrets, logs, routing, and runtime management handled by one tool.

The website exists to make that promise concrete. Success means a developer can understand what Tako is, install it, see the first path to production, and find detailed docs without marketing fog.

## Positioning

The platform layer that makes your own servers feel like a PaaS. Four claims together set Tako apart from other deploy tools and hosting platforms:

- **The whole platform runs on one VPS.** `tako-server` handles deploys, TLS, routing, processes, logs, secrets, rollbacks, and scale-to-zero. It also runs the app primitives (workflows, channels, image optimization, storage, cache, and backups), so apps need no extra services and no Docker.
- **The same platform runs locally.** `tako dev` brings that platform to the laptop, with real HTTPS on `*.test`.
- **It is agent-ready.** The primitives live in code, so an agent can migrate a project and wire features in the repo instead of provisioning services.
- **It is fast and cheap to run.** The proxy is built in Rust on Cloudflare's Pingora. Idle apps scale to zero, so many apps fit on a small VPS with predictable bills.

## Operating Context

- Brand surfaces: the website (landing, docs, blog, performance), the blog's painted illustrations, generated OG images, the GitHub repo's README banner and social preview, and the `tako` CLI's terminal output colors. A visual identity change applies to all of them, not only the website.
- The docs are moving to autogeneration (in progress as of October 2026). Hand-written pitch content, such as the former docs Intro (why Tako exists, what it does well, who it is for), belongs on the landing page.
- Installable agent skills in `.agents/skills/` and `sdk/javascript/skills/` are user-facing product surfaces.

## Brand Personality

Plainspoken, fast, owner-controlled.

The brand voice is practical and opinionated, with enough playfulness to feel human. Copy should sound like a developer explaining the shortest path that actually works. Proof matters: performance claims, platform claims, and future-looking statements should be grounded in docs, demos, benchmarks, or clear labels.

## Brand Commitments

- **The octopus mascot.** Tako (タコ) is octopus, and the octopus character stays in the logo and the blog illustrations. Its exact color is not binding.
- **Original accent colors.** Coral `#E88783` (text variant `#C4605C`), mint `#9BC4B6`, gold `#D6A63E`, and ink `#2F2A44` stay. The 77 blog illustrations and the D2 diagrams use them, so they must not need regenerating. The old warm-cream background is not part of this commitment.
- **Japanese roots, kept quiet.** "Built in Japan" and small Japanese touches stay as details. Japan is never the visual theme: the user rejected a noren-and-kamon direction (October 2026) as "too much Japanese screaming".
- **Open source.** "Open source will win" and the MIT license are part of the visible identity. Some dependencies, such as Pingora, are Apache-2.0.
- **Takosophy.** The three principles are binding: full control, unapologetically opinionated, and built for the impatient.

## Evidence on Hand

- **Public benchmark:** `/performance/` and `website/src/data/performance.ts`. One small VM served 12.5k clean HTTPS 200 RPS at 5k concurrency, with zero client errors up to 20k concurrency. The same run compares Tako with Caddy and Envoy.
- **Live demo:** https://demo.tako.sh.
- **Recording:** an asciinema recording of `tako dev` (`RXUo8Wh5i1FQIo2x`).
- **Blog:** 77 posts in `website/src/content/blog/`, with painted octopus illustrations in `website/src/assets/blog/` and D2 diagrams.
- **For agents:** `llms.txt`, `SKILLS.md`, and the agent skills.
- **Absent:** there are no customer testimonials, logos, case studies, or pricing. Do not fabricate them.

## Anti-references

Avoid generic SaaS landing-page gloss: vague productivity claims, glassy blue-purple gradients, dashboard mockups with no product meaning, and button copy that could belong to any cloud platform.

Avoid fake command-line seriousness: glowing green-on-black consoles and terminal costume. A plain, monospace, document-like site with tables and simple outlined diagrams is the chosen direction (October 2026), not a violation.

Do not name the products or sites the design took inspiration from, in copy, docs, or design records. Factual product names that belong to Tako's story (dependencies such as Pingora, supported agents, cloud providers, benchmark comparisons) are fine.

Avoid benchmark bravado without public data. Performance language needs measured context, raw reports, or a clear next action.

Avoid dashboard chrome on brand pages. The default surface is marketing and documentation, not an admin console.

The user rejected the pre-redesign site as reading AI-generated (October 2026). That site used a warm cream ground, rounded cards everywhere, pill buttons, and a colored phrase in the headline.

## Design Principles

Show the path, then let users move. Lead with install, local development, server setup, deploy, and docs links instead of abstract positioning.

Make ownership feel easy, not ideological. "Your servers" is the promise, but the design should keep the work approachable.

Use proof where confidence is needed. Performance pages, deployment docs, and technical claims should surface measurements, constraints, and links.

Keep the site playful only where it helps memory. Visual quirks and casual labels should make Tako recognizable without obscuring commands or docs.

Treat docs as product UI. Navigation, code snippets, anchors, copy buttons, and tables need the same polish as the homepage.

## Accessibility & Inclusion

Target WCAG AA contrast for text and interactive controls. Preserve keyboard access, visible focus states, semantic headings, and useful link text.

Respect reduced-motion settings. Motion can add personality, but install paths, docs, and navigation must remain usable without animation.

Keep command examples readable on small screens. Long code and configuration snippets should scroll predictably without hiding copy actions or adjacent content.
