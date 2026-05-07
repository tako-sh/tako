---
description: Create a new blog post from a topic or idea
---

$ARGUMENTS

# Blog Post

Create a new blog post for the Tako website based on an idea provided by the user.

## Input

The user provides a topic or idea. Examples:

- "write about why we don't use Docker"
- "compare Tako to Kamal"
- "announce the new secrets feature"

## Process

### Step 1 — Research

1. Read `SPEC.md` to understand current Tako capabilities and architecture.
2. Read existing blog posts in `website/src/content/blog/` to match tone and style.
3. Check memory for competitor landscape data (reference_competitor_landscape.md) for context on similar tools.
4. If the topic involves a specific feature, read the relevant source code to get details right.
5. If the topic involves competitors or external tools, do web research to get current facts (stars, versions, status).
6. **Fact-check rigorously.** Every factual claim (star counts, release dates, feature support, version numbers) must be verified against at least two independent sources. Cross-reference docs, GitHub, and web search results. If two sources disagree, dig until you find the truth. Do not publish a number you only saw once.

### Step 2 — Write

Create a new markdown file at `website/src/content/blog/{slug}.md` with this frontmatter:

```markdown
---
title: "Post Title"
date: "YYYY-MM-DDTHH:MM"
description: "A concise 1-2 sentence summary for SEO meta tags and social previews. Should be compelling and specific — not generic."
image: 9a637d413a53
---
```

The `description` field is **required** — it populates `<meta name="description">`, Open Graph, Twitter cards, and the blog listing page. Write it as a standalone sentence that makes sense in search results and social shares. Keep it under 160 characters.

Hero source images go in `website/src/assets/blog/` as compressed `.png` files. The `image` field is just the ID (no extension). Generate the hero image directly, resize the source PNG to a maximum width of 1400px while preserving aspect ratio, optimize it with `oxipng -o 4 --strip safe`, hash the optimized PNG with SHA-256, and use the first 12 hex characters as both the filename and `image` value. Astro optimizes the source image during build; do not commit generated AVIF/WebP derivatives. Landscape orientation (roughly 16:9). The site's layout displays the hero in a centered, boxed frame (max 640×360). The `image` field is optional only when image generation is unavailable or the post should intentionally ship without a hero.

Guidelines:

**Tone & voice:**

- Default (Tako-kun): First person plural ("we") or neutral. Friendly, playful, catchy — like a mascot talking to a friend who codes.
- Dan Lilienblum posts: First person singular ("I"). More personal and opinionated.
- Light humor is welcome. Forced jokes are not.
- Never aggressive, never attack other products. When mentioning competitors, be genuinely respectful — they're building cool things too. Say what Tako does differently, not what others do wrong.
- Confident but not arrogant. "We chose X because Y" not "X is obviously better than Z."
- No corporate speak, no marketing buzzwords, no "leverage", no "empower", no "game-changer".
- No filler intros ("In today's fast-paced world..."). Get to the interesting part.

**Tako's vision (weave in where relevant):**

Tako isn't just a deploy tool — it's becoming the platform layer between your code and the internet. Today it handles deployment, routing, TLS, secrets, and local dev. The roadmap includes backend primitives like WebSocket/SSE channels, queues, workflows, and image optimization — things most apps bolt on as separate services. Most competitors (Kamal, Dokku, Coolify) stop at "get your code running." Tako wants to provide the infrastructure your app needs so you don't have to.

Combined with multi-server environments and Cloudflare Argo smart routing, Tako lets you build your own edge network on cheap VPS boxes worldwide — competitive with Fly.io, but on your own hardware.

**Structure & content:**

- **Title**: include the searchable phrase a developer would actually type — concrete nouns (tool names, technologies, actions) belong in the title, not just in the body. "Deploy Node.js without Docker" beats "We ditched the whale." Clever is fine when the clever title still contains the search phrase; lean clever in the opening paragraph, not in the title. "How to X" / "X vs Y" / "Why X does Y" shapes all work — pick whichever fits the post.
- **Length**: 1,200-1,500 words. Go deep enough to be genuinely useful, then stop.
- Short intro paragraph, 2-3 sections with h2 headings, brief closing.
- Every claim about Tako must be verifiable from SPEC.md or source code.
- Code examples when they clarify. Use real Tako commands/config, not pseudocode.
- **Backlinks are mandatory.** Every post must link to at least 2-3 relevant docs pages (e.g., `/docs`, `/docs/tako-toml`, `/docs/deployment`, `/docs/cli`, `/docs/development`). Link inline where concepts are mentioned — don't save all links for the end. Also link to the GitHub repo, other blog posts, or external resources where relevant. Think of each post as an entry point that guides readers deeper into Tako's docs.
- **Use tables for structured data.** When comparing tools, listing features, or presenting any data with multiple dimensions, use Markdown tables instead of prose or bullet lists. Tables are easier to scan and make comparisons obvious.
- **Use D2 diagrams for architecture and flows.** When explaining how components connect, data flows, or multi-step processes, use ` ```d2 ` code blocks. D2 renders to inline SVG at build time via `astro-d2` (sketch mode, Shirley Temple theme). Keep diagrams simple — they should clarify, not overwhelm. Good uses: deploy pipelines, request routing, component relationships. Bad uses: anything that's clearer as a sentence.

### Step 2b — Generate and import hero image

Generate the hero image yourself using the image generation tool. Do not leave an `IMAGE PROMPT` comment in the post, do not copy anything to the clipboard, and do not ask the user to download or import the image manually. Use the prompt guidance below to create a post-specific image prompt, generate a wide illustration, import the compressed source PNG, update the post's `image:` frontmatter with the generated ID, and keep the original generated source image in place unless the user explicitly asks to delete it.

Prompt scaffold:

```text
Generate a wide illustration for a blog post hero image.

Character: A small, simple octopus matching the Tako logo direction — flat, minimal, no outlines, soft pastel coral pink body with simple dot eyes and a small curved mouth. Not 3D, not shiny, not glossy. Expressive and full of personality — eyes can squint, widen, or glance; the mouth can grin, gasp, or smirk; tentacles are always doing something. Stylized, not realistic, and not hyper-kawaii either.

Scene: [This is the most important section. Do not describe a "scene" — describe a STORY MOMENT, but describe it **purely visually, without naming any source**.

The difference: a scene is decorative ("octopus holding three servers"). A story moment is a single frame from a larger narrative the viewer already half-knows — it arrives pre-loaded with tone, stakes, and meaning because the viewer's brain fills in the rest from memory.

**CRITICAL — do not name any real-world source in the scene text.** Image generation guardrails often reject prompts that name copyrighted films, shows, franchises, characters, mascots, living artists, trademarked worlds, or recognizable brand universes — even as "inspired by" or "in the style of." Pick the story moment in your head (and the post draft), then translate it into visual ingredients only: the setting, era, costumes, lighting, poses, props, weather, color of the light. If the reader wouldn't recognize the moment without the name, the description isn't vivid enough — add more specifics rather than bolting the name back on.

Prefer references that live in public domain or genre-trope territory: classical myths (Atlas, Sisyphus, Icarus), pre-1928 paintings and woodblock prints, historical settings (medieval cathedral build, lighthouse in a storm, old switchboard room, 1960s mission control, a smoke-filled noir detective's office), and generic genre scenes (Formula 1 pit stop, safecracker at a vault dial, symphony conductor mid-downbeat, mountaineers on a summit). Avoid modern films, TV shows, animated features, comic universes, named fictional characters, and any specific living author or artist.

Then pin down for the prompt text:
  (1) WHAT MOMENT is this a frame of? (described in pure visual terms — "four caped figures crossing slim swords at the top of a stone staircase under torchlight" rather than naming the story)
  (2) WHICH beat of that moment? (the triumphant beat, the quiet-before-the-storm beat, the "oh no" beat)
  (3) What verb is each octopus physically doing? (not "standing" / "holding next to" — an active verb)
  (4) What emotional beat does each octopus carry? (determined, delighted, mischievous, proud, nervous, triumphant)
  (5) What gesture makes each beat legible? (tentacles raised, pointing, bracing, mid-throw, arms crossed)
  (6) Does the frame contain the post's literal subject, or only a metaphorical stand-in? If someone couldn't guess the post's topic from the image + title, pick a more literal moment (workflows → an actual multi-step assembly line; routing → an actual switchboard; secrets → an actual vault).

Aim for "single frame of something I recognize by its visual grammar," not "mascot standing in a scene." Whimsical props and slightly absurd juxtapositions are encouraged when they fit.]

Style requirements:
- Flat illustration with paper-like grain texture
- Light, airy, pastel tones — not saturated, not glossy, not 3D
- Color palette: coral pink (#E88783), mint teal (#9BC4B6), warm beige (#FFF9F4) background, dark purple (#2F2A44) accents
- Playful, characterful, and full of motion — warm and friendly but lively. Think children's book spread or New Yorker cover, not corporate landing page. A soft sense of movement (flying confetti, dust puffs, motion lines, tilted angles) is welcome when it fits.
- Landscape orientation, roughly 16:9. The image will be displayed in a centered, boxed frame at about 640×360 — no cropping, the original aspect ratio is preserved, so compose the whole frame to be presentable.

Output: a single image in widescreen landscape format.
```

The prompt should be specific to the post's topic — not generic. The #1 failure mode is a tidy but lifeless "object + object" composition where the octopus just stands next to some props. The fix is not more motion lines or a better pose — the fix is **telling a story**. A hero image works when it borrows a single frame from a recognizable genre moment and the image arrives pre-loaded with tone, stakes, and meaning. Without a story anchor you're just decorating; with one, you're telling.

**Pick the moment before you write the prompt.** Start by asking: _"If this blog post were a historical painting or an iconic scene, what would it depict?"_ Then take that one specific frame and cast the octopus into it. The reference should actually match the post's content and mood — don't force it, and don't default to the same reference across adjacent posts. Variety matters; context matters more.

**The image must be guessable, not decodable.** A reader should see the hero, glance at the title, and feel "yeah, that fits" — not need a paragraph of explanation to connect the visuals to the post's subject. The most common failure is picking a moment whose relevance only lands after you explain the metaphor (e.g. "the octopus has eight arms, so a solo chef represents a single process running many concurrent steps"). That's one hop too many. Prefer moments that contain the post's literal subject (workflows → an actual multi-step assembly line or switchboard; routing → an actual sorting room; secrets → an actual vault). Metaphor is fine; metaphor that needs a decoder ring is not. If you'd need to say "it's about X because Y," start over — the Y should already be visible.

**Watch for the "shared-activity trap" in comparison posts.** The most seductive failure mode for `Tako vs [Tool]` posts is pairing two octopuses doing _the same activity in different ways_ — both cooking, both performing music, both racing — where the contrast is supposed to be "cluttered station vs. tidy station" or "many instruments vs. one instrument." This almost always reads as "duet" or "two variants of the same thing," not "replace A with B." The viewer's first interpretation of two figures doing the same activity together is collaboration. For replacement/consolidation posts, either (a) focus on a single figure whose situation contains the contrast, or (b) pair two figures doing _visually opposite_ actions: one struggling under a pile vs. one holding a single item; one at a cluttered workbench vs. one at a clean counter; one juggling vs. one reading a book. The composition should read "replace this with that" before the viewer has thought about it.

**Labels are a bridge, not wallpaper.** In comparison posts, readable labels can make the subject clear, but too many labels turn the hero into a diagram. Use at most **1-2 readable labels total**, and prefer one label on the central object or competitor prop over signs on every surface. Do not add slogans, explanatory placards, repeated brand names, or tiny incidental text. The composition should carry the contrast first; labels only disambiguate what the title already says. Reserve pure-trope compositions (no labels) for posts where the subject is a concept (scale, security, workflows) rather than a named competitor.

For named-competitor posts, decide what absolutely needs a label. If the title already says "Vercel" or "PM2", the image may only need the app/runtime/object labeled (`Next.js`, `API`, `logs`) while the competitor is implied by the visual grammar. If a competitor label is needed, put it on one clean prop (a box, jersey, sign, clipboard, or tool tag), then explicitly say "no other readable text" in the prompt.

**Then describe it without naming it.** Image generation guardrails frequently reject prompts that name copyrighted films, characters, franchises, mascots, trademarked worlds, or living artists — including "inspired by" and "in the style of" phrasings. The fix isn't to argue with the guardrail — it's to write the description so vividly in visual terms (setting, era, costumes, lighting, poses, props) that the reference lands through recognition of the visual grammar, not through the proper noun. The picks in the table below are intentionally trope- and public-domain-leaning so this is natural; if you find yourself reaching for a modern film or character, translate it to its setting + costume + prop + pose before writing the prompt.

**Safe tiers to pull from, in priority order:**

1. **Japanese anime / manga visual traditions (vibe, not franchise)** — **prefer these first.** They land especially well with a developer audience and translate cleanly to visual ingredients without needing a proper noun. Examples: late-night ramen shop / yatai food stall with lanterns and steam curls, cozy pastoral hand-painted watercolor interiors, shonen action stances with radiating speed lines, pirate-crew camaraderie tableaus on sunlit ship decks, rooftop ninja leaps in moonlight, izakaya dinners under red lanterns, mountain-temple training-arc courtyards at dawn, mecha-hangar assembly bays, school-rooftop lunch scenes with wide skies, bathhouse soaks at dusk, magical-girl transformation sparkles. Describe the **visual conventions** (cel shading, expressive oversized eyes, chunky linework, dramatic low-angle, dust puffs, warm hand-painted watercolor backgrounds, golden-hour skies, Edo-print linework, steam and lantern glow, noren curtains) and the **archetypal setup** — but **do not name the studio, film, series, or character**. "A scrappy crew of seafaring adventurers gathered around a barrel of food on a sunny wooden ship deck, expressive oversized eyes, goofy-heroic poses" lands every time; "One Piece's Straw Hats at dinner" gets rejected.

2. **Universally-recognizable events, myths, and tropes** — stories that most developers across cultures will recognize without a proper noun: David vs Goliath (a tiny figure with a sling facing an armored giant), Trojan horse (a massive wooden horse rolled through stone city gates), Icarus (a winged figure soaring too close to the sun), Tower of Babel, a chariot race, a medieval cathedral build, the Apollo-era moon landing, an Olympic podium moment, a classic Edo-period great-wave woodblock composition, a lighthouse keeper braced against a storm. These work because they're part of world-shared visual vocabulary, not a single franchise.

3. **US media / Hollywood / Disney references** — **least preferred.** Most globally uneven (not all developers share the same media diet), most IP-locked (guardrails are strictest around Disney, Marvel, Star Wars, Pixar, major modern films), and often more nostalgic-to-one-generation than genuinely universal. Avoid reaching for these if tier 1 or tier 2 offers a cleaner fit. If the perfect trope genuinely lives here, translate it to pure visual ingredients and do not name the source.

Anything outside those tiers (modern TV shows, cartoons, comic universes, named fictional characters or studios, current artists) → **do not name it**. Translate it to its visual ingredients or pick a different anchor.

**These next examples are just that — examples.** They're here to unblock you when you're staring at a blank prompt, not to limit you. The right moment for a given post is almost never going to be on any pre-made list; it's whatever actually matches the specific angle, tone, and details of _that_ post. Treat the table as a sampler of the _kinds_ of places to look, then go find your own. If none of these feel right, that's expected — invent something.

| Post vibe                                 | Trope / public-domain moments to evoke (describe visually, don't name)                                                                                                                                                                    |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Distributed / coordinated / consensus     | Musketeers crossing swords on a staircase, a heist crew leaning over a blueprint table, a ragtag seafaring adventure crew on a sunlit wooden deck (anime pirate-crew tableau — cel-shaded, expressive oversized eyes, goofy-heroic poses) |
| Scale / resilience / weathering load      | A great cresting wave over a tiny boat (Edo-period woodblock style), a titan holding up the sky, lighthouse keepers in a storm, a lone figure braced against a huge shonen-anime-style energy wall (radiating speed lines, dust clouds)   |
| Speed / performance / racing              | Formula 1 pit stop mid-swap, a chariot race, a steam locomotive at full tilt, a shonen-anime-style dash pose (low-angle, speed lines, trailing dust, dramatic wind)                                                                       |
| Local dev / solo craft / quiet focus      | A wood-paneled toymaker's workshop at night, a late-night diner through a rainy window, a hand-painted watercolor pastoral cottage interior at dusk with a stew pot simmering and a cat on a rafter (anime-studio countryside vibe)       |
| Migration / escape from a heavier tool    | Tunnel-diggers crawling toward a distant light, a caravan crossing a desert at dawn, two small figures with oversized backpacks walking down a grassy slope under drifting clouds (hand-painted anime road-movie frame)                   |
| Construction / building a system up       | Medieval cathedral build with scaffolding, an ant colony cross-section, a Rube Goldberg machine in progress, a mecha-hangar assembly bay with sparks and catwalks (shonen-mecha panel)                                                    |
| Security / secrets / protection           | A safecracker at a vault dial under a single bulb, a dragon curled on a pile of gold in a cave, a temple guardian with a torch, a rooftop ninja mid-leap in moonlight (anime stealth frame — inked linework, diagonal composition)        |
| Orchestration / many things in harmony    | A symphony conductor mid-downbeat, a ballet corps in perfect diagonal, a mechanical orrery turning, a chain of bucket-passers                                                                                                             |
| Launch / announcement / triumph           | An Apollo-era rocket lift-off, an Olympic podium moment, a flag planted on a mountaintop at dawn, a shonen-style power-up stance with sparks and light rays radiating from a clenched fist                                                |
| Debugging / detective work                | A noir detective in a smoky office with a single desk lamp, a chalk-outline crime scene, a magnifying glass over a map, a trench-coated detective with exaggerated eyes and a smirk (anime-mystery frame)                                 |
| Camaraderie / team dinner / slice-of-life | A crew gathered around a steaming stew pot under lantern light, a school-rooftop lunch scene with wide skies and a train passing far below, a bathhouse-at-dusk group soak (all described as cel-shaded hand-painted anime frames)        |

Again: the table is a nudge, not a box. A post about WebSocket channels might borrow from a pneumatic-tube mailroom. A post about cold starts might borrow from a sleeping dragon waking up. A post about secrets might borrow from a classic bank heist or a diary with a lock. Pick whatever actually tells the right story for _this_ post.

**Quick self-check before finalizing the prompt:**

- **What moment is this a frame of?** (If you can't describe it in one sentence, you don't have one yet.)
- **Can someone guess the post's topic from the image + title alone?** Squint test: if a reader would need you to explain the metaphor to see the link, the image isn't doing its job. Try to get the post's literal subject into the frame (workflow steps → an actual assembly line; routing → an actual switchboard), not just a thematic stand-in.
- **Is this a "shared-activity" comparison in disguise?** If both octopuses are doing the same activity in different styles (both cooking, both performing, both racing), the image will read as "duet" or "variants," not "replacement." Pair _visually opposite_ actions (struggling/calm, cluttered/clean, many/one), or focus on a single figure whose situation contains the contrast.
- **Is there label overload?** Count readable text before finalizing the prompt. A comparison hero should usually have 0-2 labels total, no slogans, and no repeated signs. If the prompt needs many labels to make sense, the visual concept is doing too little work.
- **For vs-posts with named competitors: is the one necessary label clear?** If the post is "Tako vs [Tool]" and the competitor cannot be inferred visually, put [Tool]'s name on one labeled prop (box, jersey, sign, clipboard, tool tag), then ban all other readable text. If the title already supplies the competitor, consider labeling only the central app/runtime/object instead.
- **Did I avoid naming any film, show, book, character, franchise, mascot, brand, or living artist?** (Including "inspired by" / "in the style of.")
- **Would a reader recognize the moment from the visuals alone — the costumes, lighting, props, pose?**
- **Is there an active verb per octopus?** ("standing," "holding," "next to" don't count.)
- **Is there a facial expression per octopus, and do they differ?**
- **Are the tentacles doing something specific, or just hanging?**

If the answer to "what moment?" is fuzzy, stop and pick one before writing the scene. If the squint test fails, pick a moment that shows the post's subject more literally. If the answer to "did I avoid naming anything?" is "no," strip the names and replace them with visual detail.

After generating the image, locate the generated source file and copy it into the import workflow as a PNG:

```bash
max_w=1400
cur_w=$(sips -g pixelWidth "$tmp_src" | tail -1 | awk '{print $2}')
if [ "$cur_w" -gt "$max_w" ]; then
  sips --resampleWidth "$max_w" "$tmp_src" --out "$tmp_src" >/dev/null 2>&1
fi
oxipng -o 4 --strip safe "$tmp_src"
id=$(shasum -a 256 "$tmp_src" | cut -c1-12)
```

Move or copy the PNG to `website/src/assets/blog/${id}.png`, set `image: ${id}` in the post frontmatter, and keep the generated source image in its original generated-images location unless the user explicitly asks to delete it. The OG image route is generated by the website build into `website/dist/assets/blog/og/${slug}.png`; generated OG files should not be committed.

### Step 3 — Verify

1. Run `cd website && bun run build` to generate OG images and confirm the post builds.
2. Confirm `website/dist/assets/blog/og/${slug}.png` exists after the build.
3. Check that the post appears in the blog listing page.
4. Show the user the post title, slug, and a brief summary for approval.

## Date

Always use today's date and current time (UTC) for the post. Format: `YYYY-MM-DDTHH:MM`. Get from the system.

## Slug

Derive from the title: lowercase, hyphens, no special characters. Keep it short.
Example: "Why We Don't Use Docker" → `why-we-dont-use-docker.md`

## Competitive Landscape

Tako exists in a crowded space. Keep these tools in mind when writing — reference them when relevant, position Tako honestly against them.

**CLI-based self-hosted (most similar to Tako):**

- **Kamal** (37signals) — Docker-based deploy via SSH, custom Go proxy (kamal-proxy). 13.9k stars. Ruby. The biggest name in this space thanks to DHH.
- **Sidekick** — Go CLI, turns VPS into mini-PaaS with Docker+Traefik. 7.4k stars. Markets as "your own Fly.io."
- **Piku** — Tiniest PaaS, git-push, no Docker, Python, runs on Raspberry Pi. 6.6k stars. Closest philosophy to Tako.
- **Exoframe** — One-command Docker deploys with Traefik. JS. 1.1k stars.

**Self-hosted PaaS (web UI, heavier):**

- **Coolify** — Open-source Heroku/Vercel alternative, full web UI. PHP. 51.8k stars. Dominant in this category.
- **Dokploy** — Lighter Coolify alternative, Docker Swarm. TS. 31.7k stars.
- **Dokku** — The OG mini-Heroku, git-push+Docker+buildpacks. 31.9k stars. Battle-tested.
- **CapRover** — Web dashboard PaaS, Docker Swarm. 14.9k stars.

**Cloud PaaS (hosted competitors):**

- **Fly.io** — Edge micro-VMs, CLI-driven. Popular indie dev choice.
- **Railway** — Great DX, auto-detect, built Nixpacks/Railpack.
- **Render** — Modern Heroku replacement.
- **SST** — TypeScript IaC on AWS. 25.7k stars.

**Reverse proxies (Tako uses Pingora):**

- **Caddy** — Auto HTTPS, Go. 70.9k stars. Used by Uncloud, Ptah.sh.
- **Traefik** — Cloud-native proxy, Docker/K8s auto-config. 62.2k stars. Used by Sidekick, Exoframe.
- **Pingora** — Cloudflare's Rust proxy framework. 26.3k stars. What Tako is built on.

**Dead projects (cautionary tales):**

- Nginx Unit — ARCHIVED Oct 2025
- HashiCorp Waypoint — ARCHIVED Jan 2024

**Tako's unique positioning:**

1. No Docker required (only Piku shares this)
2. Rust + Pingora proxy (no other deploy tool uses Pingora)
3. SFTP-based deployment (others use Docker registries or git push)
4. Native process management (processes, not containers)
5. Built-in local dev with HTTPS, DNS, and proxy
6. "Everything you need to run apps on your own hardware"

## Rules

- One post per invocation.
- Don't modify existing posts unless asked.
- Don't commit — let the user review first.
- Default author is "Tako-kun" (no frontmatter `author` field needed). To post under Dan's name, add `author: dan` to the frontmatter. Author lookup is in `BlogPostLayout.astro`. Only use `author: dan` if the user explicitly asks.
