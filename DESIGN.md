---
name: "Tako"
description: "Public brand and docs site for running apps on your own servers."
colors:
  bg: "#1f1b2b"
  paper: "#2f2a44"
  ink: "#efebe6"
  muted: "#b8b2c2"
  comment: "#a39db0"
  primary: "#e88783"
  on-primary: "#1f1b2b"
  secondary: "#9bc4b6"
  gold: "#d6a63e"
  dim: "#8e879c"
  line: "rgba(239, 235, 230, 0.14)"
  line-strong: "rgba(239, 235, 230, 0.32)"
  zebra: "rgba(239, 235, 230, 0.035)"
typography:
  display:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(44px, 5.6vw, 96px)"
    fontWeight: 700
    lineHeight: 1.02
    letterSpacing: "-0.03em"
  display-closing:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(40px, 6.4vw, 104px)"
    fontWeight: 700
    lineHeight: 1
    letterSpacing: "-0.035em"
  headline-lg:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(36px, 4.6vw, 64px)"
    fontWeight: 700
    lineHeight: 1.08
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(28px, 3.2vw, 44px)"
    fontWeight: 700
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  command:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(22px, 2.4vw, 34px)"
    fontWeight: 700
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  statement:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(20px, 1.7vw, 26px)"
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "-0.02em"
  stat:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "clamp(22px, 3vw, 44px)"
    fontWeight: 700
    lineHeight: 1
    fontFeature: '"tnum"'
  lead:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "17px"
    fontWeight: 400
    lineHeight: 1.65
  body:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "16px"
    fontWeight: 400
    lineHeight: 1.65
  base:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: 1.65
  small:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  item:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "16px"
    fontWeight: 600
    lineHeight: 1.4
  label:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1
  label-sm:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.5
  code:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.65
rounded:
  hairline: "1px"
  sm: "3px"
spacing:
  row: "6px"
  sm: "12px"
  md: "16px"
  lg: "24px"
  gutter: "clamp(20px, 5vw, 80px)"
  column-gap: "clamp(40px, 5vw, 96px)"
  band: "clamp(72px, 9vw, 128px)"
  band-compact: "clamp(48px, 6vw, 80px)"
  band-inner: "1520px"
components:
  band:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.ink}"
    padding: "clamp(72px, 9vw, 128px) clamp(20px, 5vw, 80px)"
  band-raised:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    padding: "clamp(72px, 9vw, 128px) clamp(20px, 5vw, 80px)"
  band-closing:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.on-primary}"
    padding: "clamp(72px, 9vw, 128px) clamp(20px, 5vw, 80px)"
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.on-primary}"
    rounded: "{rounded.sm}"
    padding: "0 20px"
    height: "44px"
  button-ink:
    backgroundColor: "{colors.on-primary}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "0 20px"
    height: "44px"
  action:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    padding: "15px 2px"
    height: "44px"
  button-play:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.on-primary}"
    rounded: "{rounded.sm}"
    padding: "0 26px 0 22px"
    height: "clamp(52px, 4.4vw, 64px)"
  button-copy:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    typography: "{typography.label}"
    padding: "11px 8px"
  command-row:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "7px 8px 7px 16px"
    height: "52px"
  command-row-closing:
    backgroundColor: "transparent"
    textColor: "{colors.on-primary}"
    rounded: "{rounded.sm}"
    padding: "7px 8px 7px 16px"
    height: "52px"
  tab:
    backgroundColor: "transparent"
    textColor: "{colors.muted}"
    typography: "{typography.label}"
    padding: "11px 10px 10px"
  tab-active:
    textColor: "{colors.ink}"
  editor:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.ink}"
    typography: "{typography.code}"
    padding: "18px 20px"
  routes-panel:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.ink}"
    padding: "14px 18px"
  request-line:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.ink}"
    typography: "{typography.small}"
    rounded: "{rounded.sm}"
    padding: "11px 14px"
    height: "44px"
  bench-bar:
    backgroundColor: "{colors.muted}"
    rounded: "{rounded.hairline}"
    height: "clamp(18px, 2.2vw, 30px)"
  bench-bar-lead:
    backgroundColor: "{colors.secondary}"
    rounded: "{rounded.hairline}"
    height: "clamp(18px, 2.2vw, 30px)"
---

# Design System: Tako

## Overview

**Creative North Star: "The Plain Technical Document, Full Bleed"**

The site is still a technical document on dark brand ink: real copy, commands, code, a terminal recording and outlined diagrams carry it, with no decorative layer on top; painted octopus art appears in exactly three places, the hero peek, the chill octopus in who-it's-for and the footer painting. The document runs edge to edge in full-bleed bands that alternate the deep ink ground and the raised brand ink, and every block takes its own form: a statement and its answer, a star chart of principles, a pipeline of commands set as display type, a routes panel, a deploy played back as a timeline, a live request trace, an editor, a spec list, a full-width bar chart, a ruled audience list. The one face is pushed to strong scale contrast, from 12px labels to the 96px hero headline. Diagrams may move to explain a sequence; apart from the hero spot, nothing else does. The page is marked up like an annotated document: links and actions sit on a low translucent highlighter stroke that rises to a full highlight on hover, and the four main panels cast a text-mode shadow in a light-shade pattern, as a terminal would draw one.

The rhythm is long and even: generous band padding, one subject per band, text measures held to roughly 44 to 62 characters, and the figures allowed to run large. Headlines are lowercase or sentence case and say a plain thing plainly. Each band carries one accent (coral, mint or gold), and that accent colors the band's whole heading and the marks inside it, so the bands read apart without changing their grounds.

Color stays reserved. Coral is the one solid action, the focus ring and the prompt, and owns exactly one full-bleed field: the closing band. Mint is Tako's result wherever a benchmark is charted, and with gold lives in data, diagrams and code, and as a band accent in headings and marks. The painted brand art, the hero peek, the chill octopus and the painted blog and footer illustrations are the only pictorial material, and they share the four brand colors. Japan appears as a quiet detail (the "built in Japan" footer note, the Tokyo server in the deploy run), never as a theme.

**Key Characteristics:**

- Full-bleed bands alternating the deep ink ground (#1f1b2b) and brand ink (#2f2a44); a single coral closing field.
- JetBrains Mono is the only typeface, everywhere, including social cards, at strong scale contrast with tight negative tracking on display sizes.
- Each block has its own form, all drawn from the same hairlines, outlined boxes and type scale.
- One solid coral button; every other control is marked text on a translucent stroke in its band's accent.
- Every band heading but the closing one takes its band's accent as its whole color.
- One shadow: a text-mode shade, a ░ light-shade tile in muted grey offset behind the four main panels. No blur, no outlines on buttons, no underlines on links.
- Tako leads every benchmark in mint; competitors are grey.
- Diagrams explain a sequence on scroll into view and render still without JS or under reduced motion.

## Colors

Four binding brand colors (coral, mint, gold and the brand ink) on a deeper ink ground, with two off-white text tones, a dim lavender for chart competitors, and translucent off-white hairlines.

### Primary

- **Coral** (#e88783): the solid button fill, the focus ring, the `$` prompt in command rows and the deploy run, the start bars, playhead, current step number and serving wire in the deploy run, HAProxy's line in the performance charts, keyword tokens in code, the Play button on the recording, the selection highlight and the caret. As the band accent it colors the hero, commands, deploy and defaults headings and their marks, and it is the mark color on every page outside the home bands. On the closing band it becomes the ground. It is the color of "act here," never of Tako's result.

### Secondary

- **Mint** (#9bc4b6): Tako's bar, value and line in every benchmark chart and card, the check marks in the spec list, the "running" status and dot in the routes panel, the instance status in the request trace, the finalize bars and the healthy and stability-window states in the deploy run, string tokens in code, and the label of a copy button once it has copied. As the band accent it colors the why, dev and benchmark headings and their marks. Never a fill for an action.

### Tertiary

- **Gold** (#d6a63e): type and number tokens in code, the release bar in the deploy run, Envoy's line in the performance charts, the star beside the repository link, the medium and bright sparkle stars of the Takosophy constellations, their fainter glints (50%), the variable-star ring (55%), and the dashed ecliptic across the sky (32%). As the band accent it colors the Takosophy, platform and who-it's-for headings and their marks, including the request trace's live stages and matched row and the editor's open tab.

### Neutral

- **Deep Ink Ground** (#1f1b2b): the page and every unraised band, the header, the panel interiors (editor code area, routes panel, request trace, recording ground, and the deploy run on a raised band), and all text and fills on the coral field.
- **Brand Ink** (#2f2a44): raised bands (under a static paper grain, see Elevation), command rows, file-tab strips, the deploy run on the ground, and the code-highlighter background on document pages.
- **Paper Text** (#efebe6): commands, values, links, button labels on ink, spec names, headings on document pages, active tabs.
- **Muted Lavender** (#b8b2c2): secondary prose, labels, captions, footer links, the panel shade tile, every competitor bar on the home chart, Caddy in the performance charts and cards, the upload and prepare bars in the deploy run, diagram sublabels, the Latin chart names in Takosophy (85%).
- **Dim Lavender** (#8e879c): the second competitor grey: Envoy on the performance social card, and nginx as the dashed line in the performance charts.
- **Comment Lavender** (#a39db0): code comments (italic) and editor line numbers only.
- **Hairline** (14% off-white): every border, row rule and divider. **Strong Hairline** (32% off-white): the dashed VPS outline and the tako-server box in the request trace, the outer edges of the four shaded panels, the pending and waiting bars in the deploy run.
- **Zebra** (3.5% off-white): the even rows of data tables on the performance page and in blog posts.

### Named Rules

**The Band Mark Rule.** Coral is the only solid action color: the one button, the focus ring, the Play button. Marks take their band's accent at 45% (coral outside the home bands), so a link in a mint band carries a mint stroke. Mint and gold never fill a button.

**The One Coral Field Rule.** Coral fills small things (the button, the Play button) and exactly one band: the closing call to action. No other band, panel or card is coral.

**The Tako Leads in Mint Rule.** Tako's result is mint wherever it is charted: the home bar chart, the performance line charts (at a heavier 3px stroke) and the performance social card. On bar charts every competitor is grey (muted lavender, then dim lavender). Line charts give competitors distinct hues only so each line can be followed (HAProxy coral, Envoy gold, Caddy muted lavender, nginx dashed dim lavender). Green reads as more; coral next to Tako's number would read as a warning.

**The Binding Palette Rule.** Coral, mint, gold and brand ink are fixed because the painted brand art, the blog and footer illustrations and the diagrams use them. Do not tune, tint or replace them.

## Typography

**Display Font:** JetBrains Mono (with ui-monospace, SFMono-Regular, Menlo, Consolas, monospace)
**Body Font:** JetBrains Mono (same stack)
**Label/Mono Font:** JetBrains Mono (same stack)

**Character:** One monospace family set across a wide range; small sizes read typed, large sizes read like a printed command. Code sits in the same voice as prose.

### Hierarchy

- **Display** (700, clamp(44px, 5.6vw, 96px), 1.02, -0.03em): the hero headline only, lowercase, held to 12ch. Under 900px it runs clamp(40px, 10vw, 64px).
- **Display Closing** (700, clamp(40px, 6.4vw, 104px), 1, -0.035em): the closing band heading, deep ink on coral, held to 18ch.
- **Headline Large** (700, clamp(36px, 4.6vw, 64px), 1.08, -0.02em): the why band's heading, and the step up for sections that lead with a diagram or a chart (Deploy, Performance).
- **Headline** (700, clamp(28px, 3.2vw, 44px), 1.15, -0.02em): the base section title, sentence case. Headings carry no max-width: they run as wide as their column, with balanced wrapping.
- **Command** (700, clamp(22px, 2.4vw, 34px), 1.15, -0.02em): commands set as display type in the pipeline, with a 15px muted step number beside them.
- **Statement** (700, clamp(20px, 1.7vw, 26px), 1.2, -0.02em): the principle names under the Takosophy constellations.
- **Stat** (700, clamp(22px, 3vw, 44px), 1, tabular figures): benchmark values at the ends of their bars.
- **Lead** (400, 17px): the hero pitch, the Takosophy lead line and the who-it's-for lead. The why band's answer steps up to clamp(18px, 1.4vw, 20px) at 1.55 in paper text.
- **Body** (400, 16px, 1.65): section prose in muted lavender with bold runs lifted to paper text; 62ch measure.
- **Base** (400, 15px, 1.65): the document base on every page, step numbers, route URLs, deploy-run command, benchmark names.
- **Small** (400, 14px): spec details, deploy steps and host names, request-trace rows, footer links, the closing star line; buttons and actions at 14px (700 for the button, 600 for an action).
- **Item** (600, 16px, 1.4): spec names.
- **Label** (500, 13px): tabs, quiet controls (14px inside command rows), command labels, captions, routes keys and stage chips.
- **Label Small** (400, 12px, 1.5): deploy-run host details, statuses and bar labels, request-trace background notes.
- **Code** (400, 13px, 1.65): the editor and its line numbers; 12px on phones. Command rows set the command at clamp(14px, 1.15vw, 16px), 13px under 640px, 12px under 400px.

### Named Rules

**The One Face Rule.** Every role uses JetBrains Mono. Hierarchy comes from size and weight (400 / 500 / 600 / 700), with negative tracking only on 700-weight sizes of 22px and up. No second family, no italics outside code comments, no uppercase tracking.

**The Band Heading Rule.** Every band sets `data-accent` and its heading takes that accent as its whole color: hero coral, why mint, Takosophy gold, commands coral, dev mint, deploy coral, platform gold, defaults coral, benchmark mint, who-it's-for gold; the closing band stays deep ink on coral. Whole headings only: no colored words, no highlight spans, no gradient text, no label line above a heading. Headings stay lowercase or sentence case.

**The Stepped Scale Rule.** A heading takes the step its block earns: base section title by default, the larger step only for the why heading and diagram- or chart-led sections, display sizes only for the hero and the closing band.

## Layout

The page is a stack of full-bleed bands with no outer container. Each band pads clamp(72px, 9vw, 128px) top and bottom (56px under 640px) and the side gutter clamp(20px, 5vw, 80px); its content sits in an inner column of at most 1520px. A compact band, for blocks that carry little text, pads clamp(48px, 6vw, 80px); the commands band uses it. Bands alternate the ground and brand ink so each block reads as its own stretch of the scroll; the closing band is coral. The header and footer span the same gutter-to-gutter frame, so the header runs full width. Document pages (blog, performance, 404) keep the centered column of min(1100px, 100vw - 32px).

The home bands run in a fixed order, each with its ground and accent: hero (ground, coral), why (raised, mint), Takosophy (ground, gold), commands (raised, compact, coral), dev (ground, mint), deploy (raised, coral), platform (ground, gold), defaults (raised, coral), benchmark (ground, mint), who-it's-for (raised, gold), then the coral closing band.

Every two-column block uses one shared split, two equal fluid columns (1fr 1fr) at a gap of clamp(40px, 5vw, 96px), aligned to the top, so right columns line up down the page. It carries the hero's top row (the headline beside the pitch, the action row, the two command rows and the license line), the why band (its heading beside its answer), the dev band (the routes panel first, then the copy), the platform band (the copy beside the request trace), the copy under the benchmark chart, and the who-it's-for band. Takosophy sets its heading and a lead line, then three constellation entries in three columns (one column under 900px, each 120px figure beside its entry). The deploy band stacks the heading on one line and the copy under it, then the deploy run at full content width; the platform band puts the editor at full content width under its split; the hero puts the recording at full content width under its top row, clamp(72px, 8vw, 120px) below it (56px under 640px). Spec lists run three columns (two under 1100px, one under 640px). The deploy run hides its bar labels under 1180px. The hero's top row stacks under 1100px; every other split collapses to one column at 900px with a 32px gap, the pipeline drops to one column there, and the deploy run's close-up moves above its step list. Under 640px the header swaps its links for a 44px menu button, the deploy run narrows its host column to 7.5rem and its bars to 22px, the request trace wraps its URL and drops each row's description under its path, and the recording scrolls sideways at a readable 900px width instead of shrinking.

Inside a block the rhythm is small and regular: 6px between command label and row, 12px under a heading to its lead line, 14px between paragraphs, 20px under a section title, 24px before a list, 28px before an action row, 12px above a caption (28px under a shaded panel, to clear the shade's 10px drop).

## Elevation & Depth

The system is flat with one drawn exception. The four main panels (the tako dev recording, the routes panel, the deploy run and the platform editor) cast a text-mode shadow: one layer offset 10px down and 10px right behind the panel (`inset: 10px -10px -10px 10px`, z-index -1), filled with a 7 by 13 tile of tiny staggered rectangles in muted lavender (#b8b2c2) that reads as the ░ light-shade glyph and tiles seamlessly. The panel's own fill hides the overlap, so only the right and bottom edges show. The layer is generated content with no text, so assistive technology skips it, and it is identical in every band. Those panels firm their outer borders to the strong hairline where they meet the shade. Each band is `isolation: isolate`, so the shade sits on the band and under the panel. Everything else has no shadow. Depth is otherwise a single tonal step between the ground and brand ink, and the two read as different materials: raised bands (`.band-raised`) layer a static 220px SVG fractal-noise tile (feTurbulence, baseFrequency 0.9, two octaves, warm off-white at about 9% alpha) over brand ink, the same grain the painted illustrations sit on, so raised bands read as paper and ground bands, which stay flat, as ink. The grain never moves and never appears on panels or the ground. Within bands, command rows and tab strips sit on brand ink, the code area, routes panel and request trace on the deep ground, and the deploy run takes whichever tone its band does not. Separation otherwise comes from 1px hairlines. The sticky header gains a hairline under itself once the page scrolls (a 1px zero-blur rule, functionally a border).

### Shadow Vocabulary

- **Text-Mode Shade** (`.panel-shade::after`: `content: ""; position: absolute; inset: 10px -10px -10px 10px; z-index: -1; background: url("/assets/shade.svg")`, a 7×13 SVG tile of staggered rectangles filled #b8b2c2): behind the four main panels only.

### Named Rules

**The Text-Mode Shade Rule.** Only the four main panels carry it: one layer offset 10px down and right, tiled with the ░ light-shade pattern in muted lavender, the same in every band, never blurred, never colored by the accent, never on hover or state. No other panel, card, button or text gets a shadow.

**The Hairline Rule.** Structure is drawn, not lifted: borders at 14% off-white, strong borders at 32%, one tonal step of brand ink. Apart from the text-mode shade, no shadow, glow or blur is used for depth.

**The Paper Grain Rule.** Raised bands carry the static paper grain over brand ink; ground bands stay flat ink. The grain is texture, not depth: no blur, no tint per accent, no animation, and nothing inside a band takes it.

**The Run Ground Rule.** The deploy run, whose bars need a ground that is not the band's, takes brand ink on the ground and drops to the deep ground on a raised band. Other panels keep the deep ground and, where they share the band's tone, are held apart by the strong hairline and the shade.

## Shapes

One small radius (3px) on buttons, command rows, menus, the request line, the VPS outline, the tako-server box and its stage chips, the deploy run, and the performance tables and charts; deploy bars use 2px, benchmark bars and menu-button strokes 1px. Marks are square-ended strokes. The recording frame, the editor, the routes panel and rule-separated rows are square-cornered. The only round shapes are the 8px status dot in the routes panel, the 6px health probe in the deploy run and the ● before the request trace's instance status. Diagrams are outlined rectangles with 1px strokes; dashed strokes mark the outer VPS, the wires in the deploy close-up, and what has not happened yet (pending bars, the waiting release boxes). The request trace draws its routes as a text tree (├─ and └─ in muted lavender).

## Components

### Buttons and Marks

One solid button; every other control is marked text.

- **Marks:** class-less links, marked text, actions and quiet controls sit on a translucent stroke 0.3em tall along the bottom of the text, in the band accent at 45% (coral outside the home bands). On hover it rises to a full highlight behind the text (0.18s, cubic-bezier(0.22, 1, 0.36, 1)). Padded controls keep the stroke on their content box, not the hit area, and it wraps line by line. Prose links are paper text so they stand out from muted copy and stay legible on the full highlight. No underlines, dotted or otherwise, anywhere.
- **Button:** coral fill, deep-ink 700 14px label, 44px tall, 20px horizontal padding, 3px radius, no border, optional 0.9em line icon. Hover mixes 18% paper text into the coral; pressing drops it 1px. One per action row.
- **Action:** the secondary beside the button: paper-text 600 14px marked text with a 44px hit area (15px by 2px padding) and an optional icon.
- **Docs links** (Development docs, Deployment docs, How Tako works, Read the full report, and the recording's fallback link) are actions too: bold paper text on the band-accent stroke, filling on hover, set only 4px below their copy because the action padding gives the space.
- **Quiet controls** (copy): 13px 500 paper-text labels on a neutral 22% paper-text mark, 11px by 8px padding; 14px inside command rows. Copy turns its label mint once copied.
- **On the coral field:** the button becomes a deep-ink fill with paper text (hover mixes 20% paper text); actions, links and copy buttons go deep ink on an 18% deep-ink mark.
- **Focus:** a 2px coral ring at 3px offset everywhere; deep ink on the coral field.
- The 404 and performance pages use the same button and action.

### Hero

The balanced split: the display headline on the left; the pitch, the action row, the two command rows and the license line on the right. The pitch ends "Runs Node.js, Bun and Go apps, or anything with a Dockerfile." Below the split, inside `.hero-stage`, the tako dev recording runs at full content width, its top inside the first 1440 by 900 viewport.

- **Peek:** a small painted spot of the coral octopus (`src/assets/brand/peek.png`, a transparent PNG served as an optimized image, eager-loaded, empty alt and aria-hidden) peeks over the recording window's top edge. The painting is cut flat at 67.8% of its height, so its tentacles curl over the frame. It is clamp(150px, 16vw, 240px) wide and sits at the end of the headline column, under "your servers"; in one column (1100px and under) it moves to the window's right end, and under 640px it is 104px wide. It is the one animated spot (see Spots).

### Spots

Painted octopus art appears in exactly three places, each small and transparent: the hero peek (see Hero), the one animated spot; the chill octopus in who-it's-for; and the footer painting (see Navigation). `src/assets/brand/` holds only those three sources, peek.png, chill.png and footer-night.png, with provenance embedded, served through Astro Image with empty alt and aria-hidden. No other section gets a mascot: spots in the dev, deploy, platform, defaults and benchmark bands and a sage beside Takosophy were tried and removed as too much octopus; only these three read as integrated.

- **Chill octopus:** `src/assets/brand/chill.png` (480×762): the coral octopus standing upright and relaxed, half-lidded with a small knowing smile, holding a steaming coffee mug. It sits still (no hover or click) in flow under the who-it's-for lead, as the HomeSpot `spot-chill` (`.home .who-anchor .spot-chill`), clamp(64px, 6vw, 92px) wide with a 32px top margin. It is an original pose: it must never copy the clothing or design of the "Chill Guy" meme character, whose artist enforces his rights on commercial use.
- **Motion:** only the peek moves. On hover it stretches up slightly from its cut line (scale 1.03 by 1.06, 0.4s, cubic-bezier(0.22, 1, 0.36, 1)); the transform origin is the window edge, at 67.8% of its height, so the cut never shows. On click it squashes and springs back (0.6s ease-out keyframes), and three small coral SVG hearts rise from its kissing ε mouth (1.1s, staggered). Reduced motion stills it.

**The Spot Art Rule.** Painted octopus art appears on the page in exactly three places, each small and transparent: the hero peek over the recording window's edge, the chill octopus under the who-it's-for lead, and the footer painting. Never add a spot to another section, box painted art in a frame or card, run it full-bleed or give the hero a big painting column (tried and rejected as too heavy).

**The One Animated Spot Rule.** Only the hero peek moves; the chill octopus sits still, and the footer painting has only its single rise-in.

### Command Rows

The install command and the agent prompt, each a labeled, copyable row; they appear twice, in the hero and in the closing band.

- **Style:** brand-ink background, hairline border, 3px radius, 52px minimum height, the command with a coral `$` prompt, scrolling sideways without a visible scrollbar. A 13px muted label sits 6px above each row; the copy control sits at the row's end.
- **On the coral field:** transparent row with a deep-ink border and deep-ink text and prompt; the copy control is deep ink on its 18% mark.

### Why and Takosophy

The why band is a statement and its answer on the shared split: the heading "Deploying doesn't have to be dramatic." at the larger heading step in the left column, and the answer reading top to bottom in the right (0.4em top padding, so it meets the heading's first line), first the lead sentences (18 to 20px paper text, 46ch), then three muted 16px points opened by a `-`, 24px below under a 1px hairline with 20px above them. Under 900px it stacks heading, answer, points. It replaced the question "Why did deploying become so dramatic?" set across the full band with the lead and points split beneath it, which read confusingly. Takosophy follows as its own band, set as a star chart: the heading, a 17px muted lead line, then one sky behind three entries in a three-column grid (gap 40px by the shared column gap), each a constellation drawn in SVG with its principle name (statement size, paper text) 20px beneath and a 16px muted line (34ch) under that.

- **Sky:** one SVG (viewBox 1600×600, `preserveAspectRatio="xMidYMid slice"` so the arcs stay round) set as a planisphere: seven declination circles and eleven meridians around a pole far below the band, hairlines at rgba(239, 235, 230, 0.07) with a non-scaling stroke; a dashed gold ecliptic on its own tilted circle (32%, dash 2 7); 46 fixed dust stars at 32% off-white. It bleeds to the window edges through a computed `--bleed`, the band clips it horizontally (`overflow-x: clip`), and two intersected masks fade its top, bottom and sides.
- **Figures:** a key, CLAVIS (Full control), a one-way arrow, SAGITTA (Unapologetically opinionated), and a thunderbolt, FULMEN (Built for the impatient), each up to 320px wide in a 240×200 viewBox, among a few faint dust stars at 40% off-white. As on a real chart, the symbol follows brightness: faint stars (r < 2.2) are small pale dots at 85% off-white (r × 0.95); medium stars (2.2 ≤ r < 3.2) are gold pinched four-point sparkles (pinch 0.24) at r × 2.5, each tilted a deterministic ±12° so they don't look stamped; bright stars (r ≥ 3.2) are a gold sparkle at r × 3 plus a 50% glint turned 45° at r × 1.9, so they read eight-pointed. One star per figure (its `variable` index) carries a thin gold ring at 55% and r × 3.4, the chart symbol for a variable star. The figure lines are hairlines at rgba(239, 235, 230, 0.5), 0.8 wide in figure units, drawn without a non-scaling stroke so the pathLength-1 dash meets each star without a gap; they are hidden at rest, so the stars stand alone. Each figure carries its Latin chart name in 9px spaced capitals (0.24em tracking) in muted lavender at 85%.
- **Hover:** pointing at a principle draws its lines in once, star to star in figure order (0.4s ease-out, each var(--l) × 0.12s after the last), at 90% off-white, and they stay while it is hovered; leaving retracts them (0.3s ease-in). Nothing loops. On touch screens (`hover: none`) the lines show at rest. Under reduced motion they appear and disappear without transitions.
- **Narrow:** under 900px the grid drops to one column, each figure shrinks to 120px beside its entry, and the chart names drop out.

The constellations replaced the ruled principle rows (gone); a plain list, a list with a mascot beside it and classical pillars were rejected as alternatives.

### Command Pipeline (signature)

The three commands set at command size in the compact band, as a static grid of three equal columns (gap 24px by the shared column gap): each step is its muted 15px number, its command on a thin 0.14em mark at rest that fills to a full highlight on hover, and a muted one-line description 14px beneath (`tako init` "Detect runtime, create config", `tako dev` "Develop against the same platform as production", `tako deploy` "Build, upload, rolling update"). The numbers 1, 2, 3 carry the order; there are no connectors. Under 900px it drops to one column. The pipeline has no entrance animation. Coral arrows joining the commands (vertical when stacked) were removed at the user's request.

### Routes Panel

What `tako dev` prints, as a two-part panel on the ground with strong-hairline borders over the shade: a head strip with the app name in paper text and a mint dot plus "running", then a definition list with 13px muted keys in a 76px column and 15px paper URLs with muted notes, rows divided by hairlines.

### Deploy Run (signature)

One `tako deploy`, played back sped up, in a panel with a 3px radius and a strong-hairline frame over the shade, on brand ink against the ground and the deep ground on a raised band.

- **Head:** the command at 15px semibold with a coral `$`, and "3 servers in parallel" at 13px muted. There is no control.
- **Timeline:** one row per machine (your machine, then Tokyo with AWS as leader, Amsterdam on Vultr, Ashburn on Hetzner): the host at 14px semibold with a 12px muted detail, and a live 12px muted status line under it. Bars sit on a hairline axis on a shared clock and are colored by step: build paper text, upload and prepare muted lavender, release gold, start coral, finalize mint, each a 70% outline over a 30% fill; wait is a dashed strong hairline with a hatch. A pending bar is a dashed strong-hairline outline that fills left to right as a 1px coral playhead crosses it.
- **Steps:** six numbered sentences (01 to 06) at 14px; the current step's number turns coral, steps not yet reached drop to muted. A 13px muted note on failure sits under them.
- **Close-up:** "Inside start, on Tokyo": tako-server as a label with a 2px strong-hairline bus bar, dashed wires to the old and new release boxes, and a state label with a note beside each. States run not started, starting (solid outline), healthy and stability window (mint box and label, a mint meter during the window), serving (coral box, label and wire), draining (55% coral with a coral meter), stopped (hairline). A `current → releases/<hash>` line flips at finalize.
- **Playback:** about 10 seconds, looping while the timeline is on screen (40% in view): it plays through, holds the finished deploy for 3s, then starts again; it stops off screen and restarts from the beginning when it returns. While it plays, requests flow along the serving wire and a mint probe runs down the new one. Without JS or under reduced motion it shows the finished deploy, still.

### Request Trace (signature)

What one VPS serves once tako-server runs on it, as a loop of real request types beside the platform copy.

- **Request line:** a 44px log line on the deep ground with a hairline border and 3px radius: the method in the band accent at 700, the URL typed in paper text, the reply in the band accent at the right.
- **Server:** "your VPS" as a 13px muted label inside a dashed strong-hairline outline, around a tako-server box (strong hairline, deep ground) whose head carries two stage chips, `:443 TLS → route`, that light in the band accent as a request passes them.
- **Routes:** a text tree of `public/`, your app, `/_tako/image`, `/_tako/channels` and `/_tako/storages`, each with a 13px muted description. The matched row tints 12% band accent, its branch and path turn the band accent, and its status code lands in the band accent at 700. The app row carries a mint instance status ("1 running", "idle, scaled to 0", "starting").
- **Background:** secrets, workflows, backups, and logs and metrics as a two-column definition list (13px paper names, 12px muted notes) under a hairline.
- **Loop:** GET /, logo.svg 304, an `/_tako/image` resize, a channels 101 upgrade, a storages PUT, five idle minutes and a scale to 0, then a cold start. It loops only while the figure is on screen. There is no Replay, and without JS or under reduced motion it holds still on the first request, already answered.

### Editor

File tabs on a brand-ink strip over a ground-colored code area with a strong-hairline frame over the shade. Tabs are 13px medium muted labels that lift to paper text on hover; the open tab is paper text on the band-accent mark. A quiet Copy control sits in the code area's top-right corner on the editor ground, so code never shows through, and copies the open file without its line numbers. Code runs 13px at 1.65 behind a right-aligned comment-lavender line-number gutter. Tokens: keywords coral, strings mint, types and numbers gold, comments italic comment lavender, operators and punctuation muted.

### Spec List

Items in columns, each opened by a hairline rule: an 18px mint check (2px round stroke), the name at 16px semibold, a 14px muted detail.

### Bar Chart

The full-width benchmark: a name column, then bars drawn to scale with the value set at stat size at each bar's end. Tako's bar and value are mint; every competitor is muted lavender. A 13px muted caption states the measure.

### Audience List

The section title and a 17px muted lead on the left, a hairline-ruled list of 16px paper-text lines on the right, on the shared split, aligned to the top. Amounts stay plain text in the lead, never struck or enlarged.

### Data Tables and Line Charts

The performance page and blog posts share one table: 1px hairline borders, 14px text with tabular figures, a header row in 600 paper text on the tone its surface does not take (brand ink on the performance page, the ground inside blog articles), zebra even rows, numbers right-aligned. On the performance page, tables and charts sit in hairline frames with the 3px radius. Chart series follow the Tako Leads in Mint Rule: Tako mint at a heavier 3px stroke, HAProxy coral, Envoy gold, Caddy muted lavender, nginx a dashed dim-lavender line; ticks and axis labels are 12px muted tabular figures, and the legend uses short 2px swatches.

### Diagram Motion

The routes panel prints its rows in sequence. The benchmark bars grow from zero, then their values fade in. The Takosophy stars, rings and glints appear one by one (0.3s fade, 0.5s scale from 0.25, 0.06s apart), then the Latin names fade in last (0.6s), each figure 0.35s after the last; the lines do not draw on entrance and wait for hover. Each plays once as the diagram scrolls into view, on a long ease-out (cubic-bezier(0.22, 1, 0.36, 1)) with sub-second stages. The deploy run and the request trace loop while on screen instead: the deploy holds its finished state for 3s before restarting, and the request trace types each URL, steps its stages at 320ms each, and holds each answer for 1.5s. No diagram carries a Replay or Play control; only the recording has its Play button.

**The Explaining Motion Rule.** Diagrams may animate to explain a sequence: once on scroll into view, or, for the deploy run and the request trace, in a loop that runs only while it is on screen. Every diagram renders finished or still without JS or under reduced motion. Decorative motion on non-diagram content is still out, with one exception: the hero spot's hover stretch and click squash (The One Animated Spot Rule), which reduced motion stills.

### Closing Band

The one coral field: the closing heading at display-closing size in deep ink, the install and agent command rows, the ink button and a deep-ink action, and a short line with a link. Selection inverts to deep ink with coral text.

### Terminal Recording

The self-hosted tako dev recording under a single "tako dev" file tab carrying the band-accent mark, on the page ground with its palette mapped to the brand colors, square-cornered, strong-hairline framed over the shade. Before playback a 35% deep-ink scrim covers the poster frame and a centred coral "Play" button sits on it: clamp(52px, 4.4vw, 64px) tall, 3px radius, deep-ink 700 text at clamp(15px, 1.25vw, 18px) after a small CSS triangle; hover mixes 18% paper text into the coral, like the site button. Under 640px, where the terminal scrolls sideways at 900px, the button centers on the visible part of the frame. A poster frame and a link stand in until the player loads.

### Navigation

- **Header:** sticky, full width, deep-ink background, logo mark plus bold 16px wordmark, 14px medium paper-text links with no mark at rest; hover raises the full coral highlight. External and repository links carry small line icons (the star in gold).
- **Mobile:** below 640px, a 44px menu button opens a hairline-bordered deep-ink panel of 15px links, marked the same way.
- **Footer:** four columns of 14px muted links with bold paper-text titles (two under 720px); links carry no mark at rest, and hover lifts them to paper text on the full highlight. A hairline above the closing note, and the painted night illustration rising into view at the bottom: the coral octopus done for the day, sitting on top of its server tower on a dark grassy mound with a steaming mug under a few gold stars (`src/assets/brand/footer-night.png`, a transparent cut-out with provenance embedded, served through Astro Image at 560, 840 and 1120px with an empty alt), clamp(420px, 42vw, 560px) wide at an aspect ratio of 1200/659, centered on the footer's bottom edge.

### Social Cards

All cards are 1200 by 630 on the deep ink ground in JetBrains Mono.

- **Post cards:** the post's painting as a 462px band across the top, then an ink caption strip under a hairline with the title in paper text at 700 (sized to fit two lines, 28 to 44px) and the logo mark with a muted "tako.sh/blog" label. Posts without a painting fall back to the plain card.
- **Page cards:** logo and muted label at the top, the title at 700 (38 to 52px) with a muted description; the performance card adds a brand-ink panel with the real benchmark bars: Tako mint, Caddy muted lavender, Envoy dim lavender.
- **Brand art cards:** the painted master (the tako carrying a boxed app to a glowing server rack at night) carries the home OG image, the repository social preview and the README banner: logo and wordmark top left, the lowercase headline "your app belongs on your servers" in paper text on the painting's own dark ground at left, "tako.sh" in muted text at the bottom, the painting on the right.

## Do's and Don'ts

### Do:

- **Do** set every text role in JetBrains Mono and build hierarchy from size and weight.
- **Do** build pages from full-bleed bands that alternate the ground and brand ink, and give each block its own form.
- **Do** put two-column blocks on the one shared split (1fr 1fr, clamp(40px, 5vw, 96px) gap, top-aligned).
- **Do** use coral (#e88783) for the one solid button, the focus ring and the prompt, and keep it to one full-bleed field.
- **Do** keep mint and gold inside data, status, diagrams, code and band accents, and chart Tako in mint with competitors in grey.
- **Do** give every band a `data-accent` and color its whole heading and its marks with it; the closing band keeps ink on coral.
- **Do** mark links and secondary actions with the 0.3em translucent stroke that rises to a full highlight on hover; quiet controls take the neutral ink mark.
- **Do** put the text-mode shade, in muted lavender, behind the four main panels and nothing else, and clear it with a 28px caption gap.
- **Do** keep the one 3px radius and the single tonal step between ground and brand ink.
- **Do** let a diagram animate to explain a sequence, and render it finished or still without JS or under reduced motion.
- **Do** keep painted octopus art to the hero peek, the chill octopus and the footer painting, and animate only the peek.
- **Do** lay the static paper grain over raised bands so they read as paper against flat ink ground bands.
- **Do** keep Japan to small, quiet details.

### Don't:

- **Don't** use soft shadows, glows, gradients or blur, or the shade outside the four main panels.
- **Don't** outline buttons, or underline links (dotted or solid); controls are marked, not bordered.
- **Don't** use pill buttons, large rounded cards, or a light ground.
- **Don't** color single words or highlight phrases inside headlines, or put a small label line above a heading.
- **Don't** chart Tako's result in coral.
- **Don't** animate band backgrounds (drifting glyph fields, flow-line currents, request lanes) or tint band grounds per accent; tried and rejected because every band looked the same.
- **Don't** add a second typeface, or uppercase tracked labels.
- **Don't** add decorative motion to non-diagram content; outside diagrams, the footer illustration's single rise-in is the only entrance animation and the hero spot's hover and click the only interactive motion, and both respect reduced motion.
- **Don't** add a mascot or spot to any other section, copy the clothing or design of the "Chill Guy" meme character in the chill octopus, box painted art, run it full-bleed or at column size, or give the hero a big painting column.
- **Don't** alter the four binding brand colors.
