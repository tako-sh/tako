# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Register

brand

## Users

Tako is built primarily for solo builders who want to run applications on their own VPS. A few people working together should fit naturally, but large-team requirements do not drive the product.

- **Solo builders** who choose their own VPS for predictable costs, control, privacy, independence from hosting platforms, and access to a full runtime. They may be moving from Vercel or Cloudflare, or replacing a setup they assembled themselves. They want a coherent platform that one person can understand and operate.
- **The coding agents those builders delegate to** (Codex, Claude, and others) are an important access path. An agent may read the site, docs, `llms.txt`, and `SKILLS.md`, then migrate a project ("Move this project to tako.sh") or wire a feature in. Pages must work when an agent is the reader.

## Product Purpose

Tako is a complete, opinionated application platform for solo builders, running on their own VPS. It brings local development, deployment, operations, and app primitives such as workflows and channels into one system designed to stay manageable by one person.

The website exists to make that promise concrete. Success means a builder understands the whole platform, can follow one clear path from local development to production, and can find the details needed to keep an application running.

## Positioning

A complete application platform for solo builders, on your own VPS. Full runtime power and one simple way to build and run. Four claims make that promise concrete:

- **The whole platform runs on one VPS.** `tako-server` handles deploys, TLS, routing, processes, logs, secrets, rollbacks, and scale-to-zero. It also runs the app primitives (workflows, channels, image optimization, storage, cache, and backups), so apps need no extra services and no Docker.
- **The same platform runs locally.** `tako dev` brings that platform to the laptop, with real HTTPS on `*.test`.
- **It is agent-ready.** The primitives live in code, so an agent can migrate a project and wire features in the repo instead of provisioning services.
- **It is fast and cheap to run.** The proxy is built in Rust on Cloudflare's Pingora. Idle apps scale to zero, so many apps fit on a small VPS with predictable bills.

## Product Boundaries

- **Design for one operator.** Setup, development, deployment, debugging, and ongoing maintenance should remain understandable to one person. Judge simplicity across the whole lifecycle, not only the first deploy.
- **Offer one coherent workflow.** Choose thoughtful defaults and integrate the primitives. Add configuration only when it solves a concrete problem for the target user; avoid interchangeable infrastructure backends and a growing matrix of operating modes.
- **Keep full runtime power.** Users control their server and can run the processes and dependencies their applications need. Infrastructure ownership does not require Tako to expose every internal implementation choice as a setting.
- **Keep multi-server support bounded.** Running the same application on multiple servers is supported. It is not a mandate to pursue enterprise orchestration, large worker fleets, or highly customizable infrastructure management.
- **Validate the market-fit hypothesis.** This audience and direction are the product thesis. Adoption, continued use, and concrete applications running on Tako must establish market fit; a broad feature list alone does not.

## Possible Future Direction

An optional Tako account could authenticate builders and sync encrypted secrets and selected private configuration remotely. This would make setup across machines and collaboration with a second person easier, without requiring that information to be committed to Git.

This is a future possibility, not a current capability or delivery commitment. Keep it focused on the needs of one person or a few collaborators, with simple sharing and revocation rather than enterprise administration. It should preserve control of the user's VPS, and deployed applications should keep running independently of the account service.

Before implementation, define which configuration belongs remotely versus in the repository, who holds decryption keys, who can decrypt secrets, and how sharing, recovery, revocation, and export work. Do not claim end-to-end encryption until that model exists. Authentication here means access to Tako's account and sync service; application-user authentication is a separate product decision.

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
