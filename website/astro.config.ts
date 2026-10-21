import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import astroD2, { type AstroD2UserConfig } from "astro-d2";
import { readdirSync, statSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { SNIPPET_THEME } from "./src/config/snippet-theme.ts";
import { remarkD2Theme } from "./src/remark/remark-d2-theme.ts";
import { remarkDocsLinks } from "./src/remark/remark-docs-links.ts";
import sitemap from "@astrojs/sitemap";
import { context7SearchLink } from "./src/config/context7-ask.ts";
import { normalizeCanonicalPath } from "./src/utils/canonical";

const workspaceRoot = fileURLToPath(new URL("..", import.meta.url));
const websiteRoot = fileURLToPath(new URL(".", import.meta.url));
const pagesRoot = fileURLToPath(new URL("./src/pages", import.meta.url));
const contentRoot = fileURLToPath(new URL("./src/content", import.meta.url));
const defaultLastModified = statSync(path.join(websiteRoot, "public")).mtime;
const d2Config = {
  appendix: false,
  experimental: {
    useD2js: true,
  },
  inline: false,
  layout: "dagre",
  output: "d2",
  pad: 40,
  sketch: true,
  skipGeneration: false,
  theme: {
    default: "102",
    dark: false,
  },
} satisfies AstroD2UserConfig;

function walkFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const fullPath = path.join(dir, entry.name);

    if (entry.isDirectory()) {
      return walkFiles(fullPath);
    }

    return [fullPath];
  });
}

function toRoutePath(filePath: string): string | null {
  const relativePath = path.relative(pagesRoot, filePath).replaceAll(path.sep, "/");
  const withoutPageExtension = relativePath.replace(/\.(astro|md|mdx|html|js|ts)$/u, "");
  const routePath =
    withoutPageExtension === "index"
      ? "/"
      : withoutPageExtension.endsWith("/index")
        ? `/${withoutPageExtension.slice(0, -"/index".length)}`
        : `/${withoutPageExtension}`;

  if (routePath.includes("[") || routePath === "/404" || routePath === "/500") {
    return null;
  }

  if (routePath.endsWith(".xml") || routePath.endsWith(".json") || routePath.endsWith(".txt")) {
    return routePath;
  }

  return normalizeCanonicalPath(routePath);
}

const pageLastModified = new Map<string, Date>();
for (const filePath of walkFiles(pagesRoot)) {
  const routePath = toRoutePath(filePath);
  if (routePath) pageLastModified.set(routePath, statSync(filePath).mtime);
}

// Content collections (blog posts in src/content/blog/) map to /blog/{slug}
for (const filePath of walkFiles(contentRoot)) {
  const rel = path.relative(contentRoot, filePath).replaceAll(path.sep, "/");
  const match = rel.match(/^(\w+)\/(.+)\.\w+$/);
  if (match) {
    const [, collection, slug] = match;
    pageLastModified.set(
      normalizeCanonicalPath(`/${collection}/${slug}`),
      statSync(filePath).mtime,
    );
  }
}

// Static build (dist/). Cloudflare Workers serves the assets and handles installer script headers.
export default defineConfig({
  site: "https://tako.sh",
  output: "static",

  markdown: {
    processor: unified({
      remarkPlugins: [remarkD2Theme, remarkDocsLinks],
    }),
    shikiConfig: {
      theme: SNIPPET_THEME,
      langAlias: {
        d2: "plaintext",
      },
    },
  },

  vite: {
    optimizeDeps: {
      exclude: ["@resvg/resvg-js"],
    },
    server: {
      fs: {
        allow: [workspaceRoot],
      },
    },
  },

  integrations: [
    {
      name: "context7-ask",
      hooks: {
        "astro:build:done": async ({ dir }) => {
          const headersFile = new URL("_headers", dir);
          const headers = await readFile(headersFile, "utf8");
          await writeFile(
            headersFile,
            headers.replace("/*\n", `/*\n  Link: ${context7SearchLink}\n`),
          );
        },
      },
    },
    astroD2(d2Config),
    sitemap({
      serialize(item) {
        const itemUrl = new URL(item.url);
        const pathname = normalizeCanonicalPath(itemUrl.pathname);
        itemUrl.pathname = pathname;
        return {
          ...item,
          url: itemUrl.toString(),
          lastmod: (pageLastModified.get(pathname) ?? defaultLastModified).toISOString(),
        };
      },
    }),
  ],
});
