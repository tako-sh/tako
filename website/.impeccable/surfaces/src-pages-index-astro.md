---
version: 1
slug: "src-pages-index-astro"
primary_target: "src/pages/index.astro"
related_targets: []
---

# Landing page (tako.sh /)

Scope: the public landing page; its tokens are shared by the blog, the performance page and 404. Visitor mode: Persuade.

Audience and job: developers and the coding agents they delegate to, deciding whether Tako should run their apps on their own servers.
Action: copy the install command or hand the move to an agent; both carry equal weight in the first viewport.
Proof: three commands to live, the deploy to three regions, the platform primitives in code, the public benchmark, the live demo, the tako dev recording.
Constraints (v2 adds: recording in the hero, full-bleed layout, distinct block treatments, bolder scale; the user found the plain v1 boring. Later: the docs Intro pitch moved onto the landing, the deploy figure was rebuilt as an animated rollout playback, and the user allowed diagrams to animate. Then the user found v2 boring: headings now take their band's accent and the main panels sit on a dotted print shadow; animated band backgrounds and tinted grounds were tried and rejected): keep every block, the docs Intro content, the original accent colors, and the footer illustration. Japan stays a quiet detail. User steers, in order: less AI-looking, simple, our colors, human touch; dark ("light makes me blind"); simplify everything (no colored button states); a markdown-like document with very basic styling; a plain technical product site as the reference. The user approved the result ("Yes, this is it").

Chosen direction: user-pinned plain technical document on dark brand ink. The roll's noren direction was rejected.

## Direction contract

THESIS: Tako's site as a plain technical document: real copy, tables, code and outlined diagrams carry the page. Diagrams may animate once to explain a sequence; beyond them only the recording and the footer illustration move. It refuses the AI landing kit (cream ground, rounded cards, pill buttons, gradient glows, colored headline words, decorative motion).

OWN-WORLD: Deep brand-ink ground #1F1B2B alternating with brand-ink bands #2F2A44, full-bleed sections with no outer container, off-white text #EFEBE6 and secondary #B8B2C2. Coral #E88783 is the single interactive accent and owns one full-bleed field, the closing CTA band; mint #9BC4B6 and gold #D6A63E appear in data, band headings and panel shadows, never on controls. JetBrains Mono is the only typeface, used at strong scale contrast. Each block has its own form (statement band, principle rows, compact command pipeline, routes panel, rollout playback, looping request trace, editor, spec list, full-width bar chart with Tako in mint); each heading in its band's accent (coral, mint, gold); hairlines, one small radius, one shared 1fr:1fr split, controls drawn as marks (a low band-accent stroke that rises to a full highlight; one solid coral button per group), and one shadow form: a light-shade (░) dither tile in muted grey, offset 10px behind the four main panels. Painted art appears only as small transparent spot details on the ground.

STORY: A developer, or their agent, sees that Tako turns a VPS they own into the whole platform, believes it through three commands, the rollout playback across three servers and the public benchmark, then copies the install command or the agent prompt.

FIRST VIEWPORT: Full-bleed hero on the balanced split. Left: the large lowercase headline "your app belongs on your servers". Right: the subtitle with supported runtimes, the coral Get started with the Live demo and Read the docs marks, the install command and agent prompt as two copy rows, and "Open source under the MIT license." Below: the self-hosted tako dev recording at full content width, its top inside the first viewport at 1440x900, with a small painted octopus spot peeking over its top edge under "your servers".

FORM: Plain technical document, user-pinned over the roll. seed:b68d8655adc63194

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
