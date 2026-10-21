---
description: Suggest 5 blog post ideas with one or two marked as recommended
---

$ARGUMENTS

# Blog Ideas

Generate 5 fresh blog post ideas for the Tako website. Mark the strongest 1–2 as **Recommended**. Report only — do not write any posts.

## Input

The user may pass a theme, angle, or constraint as `$ARGUMENTS` (e.g. "focus on local dev", "comparisons only", "secrets feature"). If empty, range broadly across Tako's surface area.

## Process

### Step 1 — Gather context

1. Read `README.md` and the relevant pages in `website/docs/` to understand current Tako capabilities.
2. List existing posts in `website/src/content/blog/` and read titles + first paragraphs to avoid duplication and detect coverage gaps.
3. Skim recent commits (`git log --oneline -n 30`) for shipped-but-unannounced features.
4. Check memory for the platform vision (`project_platform_vision.md`) and competitor landscape if present.
5. If `$ARGUMENTS` references a feature, read the relevant source to make sure the angle is accurate.

### Step 2 — Generate ideas

Aim for variety across these post types — don't return five of the same kind:

- **Announcement** — a recently shipped feature that hasn't been written about yet
- **Deep dive** — how an existing feature works under the hood
- **Comparison** — Tako vs. a specific competitor (only if no recent post covers it)
- **Tutorial** — concrete "build X with Tako" walkthrough
- **Opinion / philosophy** — why Tako does something differently (no Docker, fetch handlers, SFTP, etc.)
- **Vision** — where the platform is heading (channels, queues, workflows, image opt, edge)

**Bias toward SEO reach.** At least **2–3 of the 5 ideas must target a high-intent search query** — the kind of phrase a developer types into Google when shopping for a tool. Strong patterns to look for:

- `open source <Tool> alternative` — Cloudflare Workers, Heroku, Pusher, Ably, AWS Lambda, Vercel, Inngest, Temporal, Sidekiq, etc.
- `self-hosted <Tool> alternative` — same target list, different intent
- `<Tool> alternative for <year>` — works for declining-but-still-searched platforms (Heroku)
- `how to deploy <Framework> to a VPS` — Next.js, SvelteKit, Remix, TanStack Start, Hono, Bun
- `self-host <Capability>` — websockets, cron, queues, workflows, real-time, edge functions
- `<Action> on deploy` — run database migrations, run release commands, etc.

Before proposing a comparison, check `website/src/content/blog/` for an existing `tako-vs-*.md` covering the same competitor. If one exists, skip that competitor unless the new angle is materially different (e.g. SEO-shaped re-framing of a lumped post).

Reject ideas that:

- Duplicate an existing post's angle (different wording on the same thesis doesn't count as new)
- Make claims that are not backed by current documentation, shipped code, or tests
- Are generic dev-tooling content with no Tako-specific hook

**Title shape**: every proposed title should contain the concrete nouns a developer would search for (tool names, technologies, actions). Comparison and deep-dive posts are naturally search-shaped; announcement and opinion titles tend to drift clever — pull them back toward literal when the topic has an obvious search phrase. A good sanity check: would a developer plausibly type this title (or something close to it) into Google?

Prefer literal over clever. "The open source Cloudflare Workers alternative" beats "Why we built our own runtime." "How to deploy Next.js to a VPS without Docker" beats "Next.js, your way." If the topic has an obvious search phrase, the title should contain it verbatim.

### Step 3 — Score and recommend

Mark 1–2 ideas as **Recommended** based on:

- **Search intent** — the title matches a real query developers type when shopping for a tool ("open source X alternative", "self-host X", "deploy X to a VPS"). This is the strongest signal.
- **Timeliness** — shipped recently, no post yet, momentum to ride
- **Coverage gap** — fills an obvious hole in the existing blog
- **Effort/payoff** — easy to write well, hard to get wrong

If two ideas tie, prefer the SEO-shaped one. If they're both SEO-shaped, prefer the one closer to a recently shipped feature.

## Output format

```
### Blog ideas

1. **Title** — _type_ ⭐ Recommended
   Angle: one sentence on the thesis or hook.
   Why now: timeliness, coverage gap, or audience reason.
   Anchor: docs page, source or test file, commit, or competitor reference.

2. **Title** — _type_
   Angle: …
   Why now: …
   Anchor: …

… (5 total)
```

After the list, add one short line noting which existing posts each idea is closest to (so the user can verify no overlap). Example:

```
**Adjacent existing posts:**
- Idea 2 sits near `pingora-vs-caddy-vs-traefik.md` but covers latency tradeoffs, not feature comparison.
```

## Rules

- Exactly 5 ideas. No more, no less.
- Mark at most 2 as **Recommended**. One is fine. Zero is not — pick the best of the five if nothing stands out.
- Every idea must cite an anchor (docs page, source or test file, commit, or named competitor). No vague "developers care about X" pitches.
- Don't propose posts that already exist under a different title — check `website/src/content/blog/` first.
- Don't write the post. This command produces ideas only; the user will invoke `/blog-post` separately for whichever they pick.
- Keep each idea to the four-line block above. No long pitches, no draft outlines.
