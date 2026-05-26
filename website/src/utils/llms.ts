export type LlmsLink = {
  title: string;
  url: string;
  description?: string;
};

export type LlmsSection = {
  title: string;
  links: LlmsLink[];
};

export const llmsSummary =
  "Tako is a self-hosted app deployment and development platform for local HTTPS development, production deploys, routing, TLS, logs, secrets, and framework-aware defaults.";

export const llmsNotes = [
  "Prefer the Quickstart when the goal is to get an app running quickly.",
  "Use the CLI reference and tako.toml reference for exact command and configuration details.",
  "Use Framework Guides and Presets for framework-specific setup.",
];

export const llmsCoreSections: LlmsSection[] = [
  {
    title: "Start Here",
    links: [
      {
        title: "Homepage",
        url: "/",
        description: "Product overview and the shortest path to the docs.",
      },
      {
        title: "Quickstart",
        url: "/docs/quickstart/",
        description: "Fastest path from install to local HTTPS development and first deploy.",
      },
      {
        title: "Docs Intro",
        url: "/docs/",
        description: "High-level introduction to what Tako does well and who it is for.",
      },
    ],
  },
  {
    title: "Core Docs",
    links: [
      {
        title: "How Tako Works",
        url: "/docs/how-tako-works/",
        description:
          "Architecture, request flow, rolling deploys, TLS, health checks, and scaling.",
      },
      {
        title: "Development",
        url: "/docs/development/",
        description: "Local development workflow, HTTPS, DNS, routes, and dev behavior.",
      },
      {
        title: "Deployment",
        url: "/docs/deployment/",
        description: "Server setup, deploy flow, scaling, secrets, and production operations.",
      },
    ],
  },
  {
    title: "Reference",
    links: [
      {
        title: "CLI Reference",
        url: "/docs/cli/",
        description: "Commands, flags, output modes, and examples.",
      },
      {
        title: "tako.toml Reference",
        url: "/docs/tako-toml/",
        description:
          "Complete app configuration reference for routes, builds, secrets, and scaling.",
      },
      {
        title: "Presets",
        url: "/docs/presets/",
        description: "Framework preset behavior and how presets merge with runtime defaults.",
      },
      {
        title: "Framework Guides",
        url: "/docs/framework-guides/",
        description:
          "Framework-specific adapter examples for Next.js, Vite, TanStack Start, and fetch handlers.",
      },
    ],
  },
  {
    title: "Project Links",
    links: [
      {
        title: "GitHub Repository",
        url: "https://github.com/lilienblum/tako",
        description: "Source code, examples, and repository docs.",
      },
      {
        title: "JavaScript SDK on npm",
        url: "https://www.npmjs.com/package/tako.sh",
        description: "Package entry for the tako.sh JavaScript and TypeScript SDK.",
      },
      {
        title: "Agent Skills",
        url: "/skills.md",
        description: "Markdown index of task-specific Tako skills for agents.",
      },
    ],
  },
];

export function renderLlmsTxt(projectName: string, sections: LlmsSection[]): string {
  const parts = [`# ${projectName}`, "", `> ${llmsSummary}`, "", ...llmsNotes, ""];

  for (const section of sections) {
    parts.push(`## ${section.title}`, "");

    for (const link of section.links) {
      const suffix = link.description ? `: ${link.description}` : "";
      parts.push(`- [${link.title}](${link.url})${suffix}`);
    }

    parts.push("");
  }

  return `${parts.join("\n").trim()}\n`;
}
