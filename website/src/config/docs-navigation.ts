/** Shared by the documentation sidebar and the root agent index. */
export const docsNavigation = [
  {
    title: "Get started",
    pages: ["index", "quickstart", "agents", "development"],
  },
  {
    title: "Framework guides",
    pages: [
      "framework-guides",
      "nextjs",
      "tanstack-start",
      "vite",
      "fetch-handler",
      "go-http",
      "gin",
      "echo",
      "chi",
      "axum",
    ],
  },
  {
    title: "Docker",
    pages: ["docker", "dockerfile", "docker-workflows"],
  },
  {
    title: "Primitives",
    pages: ["secrets", "storage", "images", "cache", "channels", "presence", "stores", "workflows"],
  },
  {
    title: "Deploy and operate",
    pages: ["servers", "deployment", "routing", "operations", "backups", "troubleshooting"],
  },
  { title: "Reference", pages: ["cli", "tako-toml", "presets", "how-tako-works"] },
];

export function docsRoute(page: string): string {
  return page === "index" ? "/docs" : `/docs/${page}`;
}
