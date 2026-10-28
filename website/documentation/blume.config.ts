import { defineConfig } from "blume";
import { docsNavigation } from "../src/config/docs-navigation.ts";
import { SNIPPET_THEME } from "../src/config/snippet-theme.ts";
import { siteEntityIds } from "../src/config/site-identity.ts";
import { llmsNotes } from "../src/utils/llms.ts";

export default defineConfig({
  title: "Tako",
  description: "Develop locally with HTTPS and deploy apps to your own servers.",
  content: { root: "../docs" },
  lastModified: "git",
  deployment: { site: "https://tako.sh", base: "/docs" },
  logo: { image: "../public/assets/logo.svg", text: "tako", href: "https://tako.sh" },
  github: { owner: "tako-sh", repo: "tako", branch: "master", dir: "website/documentation" },
  navigation: {
    sidebar: docsNavigation.map((group) => ({
      label: group.title,
      items: group.pages.map((page) => (page === "index" ? "/" : `/${page}`)),
    })),
    actions: [{ label: "Website", href: "https://tako.sh" }],
  },
  theme: {
    mode: "dark",
    toggle: false,
    accent: { light: "#a64743", dark: "#e88783" },
    background: { light: "#fff9f4", dark: "#1f1b2b" },
    radius: "sm",
    fonts: {
      display: "jetbrains-mono",
      body: "jetbrains-mono",
      mono: "jetbrains-mono",
    },
  },
  markdown: {
    code: {
      // Match the main site's ink editor, including when printing.
      theme: { light: SNIPPET_THEME, dark: SNIPPET_THEME },
    },
  },
  seo: {
    // The parent Astro site owns the domain's robots policy and blog feed.
    robots: false,
    rss: { enabled: false },
    sitemap: true,
    structuredData: true,
    datePublished: "git",
    jsonLd: {
      websiteId: siteEntityIds.website,
      organizationId: siteEntityIds.organization,
    },
    metatags: { "theme-color": "#1f1b2b" },
    x: { handle: "@lilienblum" },
    og: {
      enabled: true,
      logo: "../public/assets/logo.svg",
      palette: {
        background: "#1f1b2b",
        foreground: "#efebe6",
        accent: "#e88783",
        muted: "#b8b2c2",
        border: "#3d3756",
      },
    },
  },
  agents: {
    agentReadability: true,
    api: true,
    webmcp: true,
    // The root robots file owns usage preferences; do not declare a second policy.
    contentSignals: false,
    llmsTxt: {
      enabled: true,
      details: [
        "Use Tako to develop with local HTTPS and deploy applications to your own servers.",
        "Install the CLI: `curl -fsSL https://tako.sh/install.sh | sh`. Install the JavaScript SDK with `bun add tako.sh`.",
        "Start with `tako init` and `tako dev`. Set up coding-agent skills and the local MCP server with `tako agents`.",
        ...llmsNotes,
      ].join("\n\n"),
    },
    // Publish maintained SDK/workflow skills instead of a generated replacement for `tako`.
    skills: "../../sdk/javascript/skills",
    skillMd: false,
    mcp: { enabled: false },
    catalog: {
      queries: {
        "docs:llms-txt": [
          "How do I run a local HTTPS app with Tako?",
          "How do I deploy a Bun, Node.js, Go, or Rust app to my own server?",
          "How do I add live data, presence, or durable workflows?",
        ],
        "skill:tako": [
          "Set up Tako development and deploy to a VPS",
          "Debug a failed Tako deployment or local dev server",
        ],
      },
    },
  },
});
