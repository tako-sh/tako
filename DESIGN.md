---
name: "Tako"
description: "Public brand and docs site for running apps on your own servers."
colors:
  bg: "oklch(94.2% 0.018 72)"
  paper: "oklch(96.4% 0.014 72)"
  panel: "color-mix(in oklch, var(--paper) 78%, var(--bg))"
  panel-strong: "color-mix(in oklch, var(--paper) 90%, var(--bg))"
  panel-glass: "color-mix(in srgb, var(--paper) 78%, transparent)"
  ink: "#2f2a44"
  primary: "#e88783"
  primary-text: "#c4605c"
  secondary: "#9bc4b6"
  gold: "#d6a63e"
  line: "rgba(47, 42, 68, 0.16)"
  muted: "rgba(47, 42, 68, 0.74)"
typography:
  display:
    fontFamily: "Poppins, Nunito, sans-serif"
    fontSize: "clamp(38px, 8vw, 72px)"
    fontWeight: 700
    lineHeight: 1
    letterSpacing: "0.01em"
  headline:
    fontFamily: "Poppins, Nunito, sans-serif"
    fontSize: "clamp(28px, 4.2vw, 36px)"
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: "0"
  title:
    fontFamily: "Poppins, Nunito, sans-serif"
    fontSize: "clamp(20px, 3.5vw, 28px)"
    fontWeight: 600
    lineHeight: 1.3
    letterSpacing: "0.01em"
  body:
    fontFamily: "Nunito, Segoe UI, sans-serif"
    fontSize: "clamp(16px, 1.5vw, 19px)"
    fontWeight: 400
    lineHeight: 1.55
  label:
    fontFamily: "IBM Plex Mono, ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, Liberation Mono, Courier New, monospace"
    fontSize: "12px"
    fontWeight: 700
    lineHeight: 1.25
    letterSpacing: "0.08em"
  script:
    fontFamily: "Caveat, cursive"
    fontSize: "clamp(30px, 5vw, 44px)"
    fontWeight: 500
    lineHeight: 1
rounded:
  xs: "2px"
  sm: "8px"
  md: "14px"
  lg: "18px"
  panel: "30px"
  pill: "999px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "14px"
  lg: "24px"
  xl: "36px"
  section: "clamp(44px, 6vw, 58px)"
components:
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "12px 26px"
    typography: "{typography.body}"
  button-soft:
    backgroundColor: "rgba(232, 135, 131, 0.2)"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "8px 18px"
    typography: "{typography.body}"
  docs-sheet:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.ink}"
    rounded: "{rounded.panel}"
    padding: "clamp(18px, 4vw, 36px)"
  feature-chip:
    backgroundColor: "transparent"
    textColor: "rgba(47, 42, 68, 0.72)"
    rounded: "{rounded.pill}"
    padding: "7px 14px"
    typography: "{typography.label}"
---

# Design System: Tako

## 1. Overview

**Creative North Star: "The Friendly Server Bench"**

Tako's visual system should feel like a hands-on workbench for self-hosted software: warm enough to invite a first deploy, structured enough to trust with production, and specific enough that it cannot be mistaken for a generic cloud tool. The current website already sets this direction with a soft OKLCH page background, coral and mint accents, rounded panels, command snippets, public benchmark tables, and small playful details.

The brand is not trying to look like a dark terminal or a corporate deployment dashboard. It explains the path, shows real commands, and uses tactile surfaces to make server ownership feel close and manageable. Documentation pages inherit the same voice: calm surfaces, strong code readability, sticky navigation, copy actions, and measured hierarchy.

**Key Characteristics:**

- Warm neutral canvas with coral, mint, and gold accents.
- Rounded tactile panels, not floating enterprise dashboards.
- Poppins headings, Nunito body, IBM Plex Mono for commands and metadata, Caveat for rare handwritten emphasis.
- Copy that is short, direct, and grounded in real actions.
- Motion used as feedback and memory, with reduced-motion fallbacks.

## 2. Colors

The palette is a restrained warm workbench with three named accents: coral for action, mint for helpful structure, and gold for highlights or proof moments.

### Primary

- **Tako Coral** (`primary`): Use for primary buttons, focus rings, active docs nav, install-path emphasis, and the main brand accent. It should be present but not everywhere.
- **Coral Ink** (`primary-text`): Use when coral needs to read as text or an inline accent. Prefer this over pale coral for body-sized text.

### Secondary

- **Server Mint** (`secondary`): Use for secondary proof, helpful states, selected copy success, and docs navigation hover states.

### Tertiary

- **Benchmark Gold** (`gold`): Use sparingly for performance proof, GitHub stars, numbered workflow accents, and warm highlights.

### Neutral

- **Warm Console Background** (`bg`): Page background. It carries warmth without becoming parchment.
- **Paper Surface** (`paper`): Code blocks, command cards, and surfaces that need a little more contrast than panels.
- **Panel Surface** (`panel`, `panel-strong`, `panel-glass`): Main content panels, docs shells, sticky nav, and overlays.
- **Tako Ink** (`ink`): Primary text and icon color.
- **Soft Rule** (`line`): Dividers, panel borders, table borders, and quiet containment.
- **Readable Muted Ink** (`muted`): Body copy and secondary text. Keep it dark enough for WCAG AA on tinted surfaces.

### Named Rules

**The Coral Earns It Rule.** Coral marks actions, active states, and specific proof. Do not wash the page in coral just to make it feel branded.

**The Mint Helps Rule.** Mint is for assistance and structure: success, sidebar hover, command chrome, and diagrams. It should not compete with the primary CTA.

**The No Generic Cloud Gradient Rule.** Never introduce blue-purple SaaS gradients. The brand color world is coral, mint, gold, ink, and warm neutral.

## 3. Typography

**Display Font:** Poppins with Nunito fallback.
**Body Font:** Nunito with Segoe UI fallback.
**Label/Mono Font:** IBM Plex Mono with system monospace fallback.
**Accent Script:** Caveat for rare, deliberately casual labels.

**Character:** The pairing is round, direct, and approachable. Poppins gives headings a clean product rhythm; Nunito keeps docs and marketing copy friendly; IBM Plex Mono is reserved for commands, paths, metadata, and charts.

### Hierarchy

- **Display** (700, `clamp(38px, 8vw, 72px)`, `1`): Homepage hero and major brand statements only.
- **Headline** (700, `clamp(28px, 4.2vw, 36px)`, `1.2`): Docs h1, page titles, and high-level section headings.
- **Title** (600, `clamp(20px, 3.5vw, 28px)`, `1.3`): Philosophy statements, card titles, and dense section leads.
- **Body** (400-700, `clamp(16px, 1.5vw, 19px)`, `1.55`): Docs and explanatory content. Cap long prose around 65-75ch.
- **Label** (700, `12px`, `0.08em`, uppercase only for short labels): Kicker text, chart labels, command captions, and metadata.
- **Script Accent** (500, `clamp(30px, 5vw, 44px)`, `1`): One-off human notes such as "Takosophy", badges, and small playful markers.

### Named Rules

**The Mono Has a Job Rule.** Use IBM Plex Mono for code, commands, charts, and metadata. Do not use it as a generic "developer" costume.

**The Script Is Rare Rule.** Caveat works because it appears in small, memorable places. Do not make it a section system.

**The Docs Stay Readable Rule.** Documentation body copy must prioritize scan speed, heading anchors, line length, and code readability over brand flourish.

## 4. Elevation

Tako uses a hybrid of tonal layering, borders, and small shadows. Large surfaces get a soft ambient lift; small controls mostly use borders, color shifts, and one-pixel or inset shadows. Depth should feel tactile, not glossy.

### Shadow Vocabulary

- **Panel Lift** (`0 18px 45px rgba(47, 42, 68, 0.1)`): Main hero, docs sheet, blog sheet, and page-level panels.
- **Primary Lift** (`0 6px 16px rgba(196, 96, 92, 0.22)`): Primary CTA at rest.
- **Primary Hover Lift** (`0 8px 20px rgba(196, 96, 92, 0.28)`): Primary CTA hover.
- **Control Hover Lift** (`0 2px 8px rgba(47, 42, 68, 0.1)`): Pills, copy buttons, and light interactive controls.
- **Modal Lift** (`0 30px 80px rgba(24, 21, 39, 0.28)`): Heavy overlays only.

### Named Rules

**The Border Carries Structure Rule.** Use borders and tonal surfaces first. Add shadows only when an element needs actual lift or state feedback.

**The No Ghost-Card Rule.** Do not pair a thin border with a wide decorative shadow on repeated cards. Pick structure or lift, not both.

## 5. Components

### Buttons

- **Shape:** Full pill for actions (`999px`), compact height, no squared-off primary buttons.
- **Primary:** Coral fill with ink text, 1.5px coral border, `12px 26px` padding, 700 Nunito, and a soft coral shadow.
- **Hover / Focus:** Hover lifts by `translateY(-1px)` and deepens coral. Focus uses a 2px coral outline with 2px offset.
- **Secondary / Ghost:** Use soft coral or panel-glass backgrounds with explicit borders. Keep labels lowercase when matching the homepage CTA voice.

### Chips

- **Style:** Mono labels, pill shape, transparent or lightly tinted background, 1.5px border.
- **State:** Active chips lift by 1px and use the category tint. Hover should add feedback without making the filter look like a primary action.

### Cards / Containers

- **Corner Style:** Main sheets use the established panel radius (`30px`). Smaller cards use `14px` to `20px`.
- **Background:** Main panels use `panel`; code and command cards use `paper`; sidebars and nested surfaces use `panel-strong`.
- **Shadow Strategy:** Main page panels can use Panel Lift. Repeated list items should be mostly flat with hover background changes.
- **Border:** 2px soft ink line for major panels, 1px to 1.5px for controls and chart cards.
- **Internal Padding:** Use fluid padding for major sheets (`clamp(18px, 4vw, 36px)`) and compact padding for controls (`7px 14px`, `8px 18px`, `14px 16px`).

### Inputs / Fields

The public website has few form fields. If adding one, follow docs controls: panel or paper background, 1.5px to 2px soft border, 8px to 14px radius, 2px coral focus outline, and body-size Nunito text. Placeholder text must meet 4.5:1 contrast.

### Navigation

Desktop navigation is sticky, compact, and text-first. The header starts transparent and becomes panel-glass with a soft border after scroll. Links use Nunito 600 and underline on hover. The GitHub action is a gold-tinted pill with a star icon. Mobile navigation uses a 44px hamburger, a blurred panel dropdown, and `inert` when closed.

### Documentation Shell

Docs use a large rounded sheet with a sticky sidebar, breadcrumb, optional mobile "show sections" toggle, heading anchors, copy buttons on code blocks, readable tables, and inline code pills. Code blocks are paper surfaces with IBM Plex Mono and horizontal scrolling.

### Performance Charts

Charts use compact panel cards, mono labels, ink grid lines, coral emphasis for Tako, and scrollable SVGs on narrow viewports. The chart should feel like public evidence, not decoration.

## 6. Do's and Don'ts

### Do:

- **Do** preserve the existing coral, mint, gold, ink, and warm neutral palette unless the user explicitly asks for a redesign.
- **Do** make install and deploy actions easy to find, especially on the homepage and quickstart path.
- **Do** use public data, raw links, and method notes when making performance claims.
- **Do** keep docs controls keyboard-accessible, visible on focus, and readable on mobile.
- **Do** use playful details sparingly: one "Takosophy" or badge-level moment is enough.

### Don't:

- **Don't** use generic SaaS landing-page gloss: vague productivity copy, glassy blue-purple gradients, or anonymous dashboard mockups.
- **Don't** use a terminal-only tech aesthetic. Commands matter, but the whole site should not become a black console.
- **Don't** make benchmark bravado without public data. Tie claims to reports, methodology, or exact measurements.
- **Don't** put dashboard chrome on brand pages. Marketing and docs need reading paths, not app-shell furniture.
- **Don't** add new colored side-stripe callouts. Use full borders, tinted backgrounds, icons, or inline emphasis instead.
- **Don't** repeat tiny uppercase eyebrows above every section. Use labels only when they add orientation.
