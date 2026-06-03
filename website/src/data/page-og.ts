import { normalizeCanonicalPath } from "../utils/canonical";

export type PageOgImage = {
  path: string;
  slug: string;
  label: string;
  title: string;
  description: string;
};

export const pageOgImages = [
  {
    path: "/blog/",
    slug: "blog",
    label: "tako.sh/blog",
    title: "Tako Blog: Self-Hosted App Notes",
    description: "Updates, ideas, and progress from the Tako project.",
  },
  {
    path: "/performance/",
    slug: "performance",
    label: "tako.sh/performance",
    title: "Tako Performance Benchmarks for Self-Hosted Apps",
    description:
      "Benchmark results with raw proxy throughput, p99 latency, CPU, memory, channels, workflows, and methodology.",
  },
  {
    path: "/docs/",
    slug: "docs",
    label: "tako.sh/docs",
    title: "Tako Documentation for Self-Hosted Apps",
    description:
      "Docs for local HTTPS development, production deploys, routing, TLS, logs, secrets, and more.",
  },
  {
    path: "/docs/quickstart/",
    slug: "docs/quickstart",
    label: "tako.sh/docs",
    title: "Tako Quickstart: Deploy Your First App",
    description:
      "Install the CLI, run local HTTPS development, and deploy your first app to your own server.",
  },
  {
    path: "/docs/framework-guides/",
    slug: "docs/framework-guides",
    label: "tako.sh/docs",
    title: "Tako Framework Guides for App Deploys",
    description:
      "Framework-specific Tako guides for Next.js, Astro, SvelteKit, Nuxt, TanStack Start, and more.",
  },
  {
    path: "/docs/how-tako-works/",
    slug: "docs/how-tako-works",
    label: "tako.sh/docs",
    title: "How Tako Works for Self-Hosted Apps",
    description:
      "How Tako handles local development, rolling deploys, TLS, health checks, request routing, and scaling.",
  },
  {
    path: "/docs/cli/",
    slug: "docs/cli",
    label: "tako.sh/docs",
    title: "Tako CLI Reference for Deploys and Dev",
    description:
      "Complete command reference for init, dev, deploy, servers, secrets, storage, status, logs, and flags.",
  },
  {
    path: "/docs/tako-toml/",
    slug: "docs/tako-toml",
    label: "tako.sh/docs",
    title: "tako.toml Reference for App Configuration",
    description:
      "Routes, runtime settings, builds, secrets, scaling, environments, and deployment configuration.",
  },
  {
    path: "/docs/presets/",
    slug: "docs/presets",
    label: "tako.sh/docs",
    title: "Tako Framework Presets and Runtime Defaults",
    description:
      "Framework-specific defaults for entrypoints, static assets, and dev commands across supported frameworks.",
  },
  {
    path: "/docs/development/",
    slug: "docs/development",
    label: "tako.sh/docs",
    title: "Tako Local Development with HTTPS",
    description:
      "Trusted HTTPS, custom .test domains, hot reload, variants, and a persistent local background daemon.",
  },
  {
    path: "/docs/deployment/",
    slug: "docs/deployment",
    label: "tako.sh/docs",
    title: "Self-Hosted Deployment",
    description:
      "Deploy apps on your own servers with setup, rolling deploys, scaling, secrets, and production operations.",
  },
  {
    path: "/docs/troubleshooting/",
    slug: "docs/troubleshooting",
    label: "tako.sh/docs",
    title: "Tako Troubleshooting for Deploys and Dev",
    description:
      "Common deploy failures, TLS issues, runtime errors, server status problems, and diagnostics.",
  },
] satisfies PageOgImage[];

const pageOgByPath = new Map(pageOgImages.map((page) => [normalizeCanonicalPath(page.path), page]));

export function getPageOgImageForPath(pathname: string, site: URL | undefined): string | undefined {
  const page = pageOgByPath.get(normalizeCanonicalPath(pathname));
  if (!page) return undefined;
  return new URL(`/assets/og/${page.slug}.png`, site).href;
}
