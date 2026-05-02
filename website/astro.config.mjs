import { defineConfig } from "astro/config";
import { readdirSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { SNIPPET_THEME } from "./src/config/snippet-theme.js";
import { remarkD2Theme } from "./src/remark/remark-d2-theme.js";
import astroD2 from "astro-d2";
import sitemap from "@astrojs/sitemap";

const workspaceRoot = fileURLToPath(new URL("..", import.meta.url));
const websiteRoot = fileURLToPath(new URL(".", import.meta.url));
const pagesRoot = fileURLToPath(new URL("./src/pages", import.meta.url));
const contentRoot = fileURLToPath(new URL("./src/content", import.meta.url));
const defaultLastModified = statSync(path.join(websiteRoot, "public")).mtime;

function normalizeCanonicalPath(pathname) {
  if (pathname === "/" || pathname === "/index.html") {
    return "/";
  }

  if (pathname.endsWith("/index.html")) {
    const directoryPath = pathname.slice(0, -"/index.html".length);
    return directoryPath.endsWith("/") ? directoryPath : `${directoryPath}/`;
  }

  if (pathname.endsWith(".html")) {
    return pathname.slice(0, -".html".length) || "/";
  }

  const segment = pathname.split("/").pop() ?? "";
  if (pathname.length > 1 && !pathname.endsWith("/") && !segment.includes(".")) {
    return `${pathname}/`;
  }

  return pathname;
}

function walkFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const fullPath = path.join(dir, entry.name);

    if (entry.isDirectory()) {
      return walkFiles(fullPath);
    }

    return [fullPath];
  });
}

function toRoutePath(filePath) {
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

const pageLastModified = new Map(
  walkFiles(pagesRoot)
    .map((filePath) => {
      const routePath = toRoutePath(filePath);
      return routePath ? [routePath, statSync(filePath).mtime] : null;
    })
    .filter(Boolean),
);

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
    remarkPlugins: [remarkD2Theme],
    shikiConfig: {
      theme: SNIPPET_THEME,
    },
  },

  vite: {
    server: {
      fs: {
        allow: [workspaceRoot],
      },
    },
  },

  integrations: [
    astroD2({
      experimental: {
        useD2js: true,
      },
      sketch: true,
      theme: { default: "102", dark: false },
      pad: 40,
      skipGeneration: false,
    }),
    sitemap({
      serialize(item) {
        const itemUrl = new URL(item.url);
        const pathname = normalizeCanonicalPath(itemUrl.pathname);
        itemUrl.pathname = pathname;
        return {
          ...item,
          url: itemUrl.toString(),
          lastmod: pageLastModified.get(pathname) ?? defaultLastModified,
        };
      },
    }),
  ],
});
