import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { tmpdir } from "node:os";

import {
  createNextjsAdapter,
  createNextjsFetchHandler,
  shutdownManagedNextjsServers,
  withTako,
} from "../src/nextjs";
import imageLoader from "../src/nextjs/image-loader";

let rootDir = "";

async function readText(relPath: string): Promise<string> {
  return await readFile(path.join(rootDir, relPath), "utf8");
}

describe("tako Next.js adapter", () => {
  beforeEach(async () => {
    rootDir = await mkdtemp(path.join(tmpdir(), "tako-nextjs-"));
  });

  afterEach(async () => {
    await shutdownManagedNextjsServers();
    if (rootDir) {
      await rm(rootDir, { recursive: true, force: true });
    }
  });

  test("withTako configures standalone output and adapter path", () => {
    const config = withTako({
      images: {
        remotePatterns: [{ hostname: "cdn.example.com", protocol: "https" }],
      },
      experimental: {
        typedRoutes: true,
      },
    });

    expect(config.output).toBe("standalone");
    expect(config.experimental?.typedRoutes).toBe(true);
    expect(path.isAbsolute(config.adapterPath!)).toBe(true);
    expect(config.adapterPath).toEndWith(path.join("src", "nextjs", "index.ts"));
    expect(config.images?.loaderFile).toBe(path.join("src", "nextjs", "image-loader.ts"));
    expect(config.images).toEqual({
      remotePatterns: [{ hostname: "cdn.example.com", protocol: "https" }],
      loader: "custom",
      loaderFile: config.images?.loaderFile,
      deviceSizes: [320, 640, 960, 1200, 1920],
      imageSizes: [],
    });
  });

  test("imageLoader returns Tako public optimizer URLs for next/image", () => {
    expect(imageLoader({ src: "/images/hero.jpg", width: 1200 })).toBe(
      "/_tako/image?src=%2Fimages%2Fhero.jpg&w=1200",
    );
    expect(imageLoader({ src: "/images/hero.jpg", width: 640, quality: 80 })).toBe(
      "/_tako/image?src=%2Fimages%2Fhero.jpg&w=640&q=80",
    );
  });

  test("adapter writes Tako wrapper and copies standalone assets", async () => {
    await mkdir(path.join(rootDir, ".next", "standalone"), { recursive: true });
    await mkdir(path.join(rootDir, ".next", "static"), { recursive: true });
    await mkdir(path.join(rootDir, "public"), { recursive: true });
    await writeFile(path.join(rootDir, ".next", "standalone", "server.js"), "console.log('ok');");
    await writeFile(path.join(rootDir, ".next", "static", "chunk.js"), "chunk");
    await writeFile(path.join(rootDir, "public", "logo.svg"), "logo");

    const adapter = createNextjsAdapter();
    await adapter.onBuildComplete?.({
      routing: {
        beforeMiddleware: [],
        beforeFiles: [],
        afterFiles: [],
        dynamicRoutes: [],
        onMatch: [],
        fallback: [],
        shouldNormalizeNextData: true,
        rsc: {},
      },
      outputs: {
        pages: [],
        appPages: [],
        pagesApi: [],
        appRoutes: [],
        prerenders: [],
        staticFiles: [],
      },
      projectDir: rootDir,
      repoRoot: rootDir,
      distDir: ".next",
      config: {
        output: "standalone",
      },
      nextVersion: "16.2.2",
      buildId: "build-id",
    });

    const wrapper = await readText(".next/tako-entry.mjs");
    expect(wrapper).toContain('from "tako.sh/nextjs"');
    expect(wrapper).toContain('new URL("./standalone/server.js", import.meta.url)');
    expect(wrapper).toContain("fetch.ready = ready;");
    expect(await readText(".next/standalone/public/logo.svg")).toBe("logo");
    expect(await readText(".next/standalone/.next/static/chunk.js")).toBe("chunk");
  });

  test("adapter falls back to next start when standalone output is missing", async () => {
    await mkdir(path.join(rootDir, ".next"), { recursive: true });

    const adapter = createNextjsAdapter();
    await adapter.onBuildComplete?.({
      routing: {
        beforeMiddleware: [],
        beforeFiles: [],
        afterFiles: [],
        dynamicRoutes: [],
        onMatch: [],
        fallback: [],
        shouldNormalizeNextData: true,
        rsc: {},
      },
      outputs: {
        pages: [],
        appPages: [],
        pagesApi: [],
        appRoutes: [],
        prerenders: [],
        staticFiles: [],
      },
      projectDir: rootDir,
      repoRoot: rootDir,
      distDir: ".next",
      config: {
        output: "standalone",
      },
      nextVersion: "16.2.2",
      buildId: "build-id",
    });

    const wrapper = await readText(".next/tako-entry.mjs");
    expect(wrapper).toContain('new URL("../node_modules/next/dist/bin/next", import.meta.url)');
    expect(wrapper).toContain('argv: ["start"]');
    expect(wrapper).toContain('cwd: new URL("..", import.meta.url)');
  });

  test("fetch handler proxies requests to the managed server", async () => {
    const fetchCalls: Array<{ input: URL; init?: RequestInit & { duplex?: "half" } }> = [];
    const handler = createNextjsFetchHandler(path.join(rootDir, "server.mjs"), {
      unstable_testing: {
        ensureServer: async () => 4010,
        fetchImplementation: async (input, init) => {
          fetchCalls.push({ input: input as URL, init });
          return new Response(
            JSON.stringify({
              method: init?.method,
              host: new Headers(init?.headers).get("host"),
              forwardedHost: new Headers(init?.headers).get("x-forwarded-host"),
              forwardedProto: new Headers(init?.headers).get("x-forwarded-proto"),
              forwardedPort: new Headers(init?.headers).get("x-forwarded-port"),
            }),
            {
              status: 200,
              headers: {
                "content-type": "application/json",
              },
            },
          );
        },
      },
    });
    const response = await handler(
      new Request("https://example.com:8443/hello?name=tako", {
        method: "POST",
        headers: {
          "content-type": "text/plain",
          host: "example.com:8443",
        },
        body: "proxy-body",
        duplex: "half",
      }),
      {},
    );

    expect(response.status).toBe(200);
    expect(fetchCalls).toHaveLength(1);
    expect(fetchCalls[0]?.input.toString()).toBe("http://127.0.0.1:4010/hello?name=tako");
    expect(await response.json()).toEqual({
      method: "POST",
      host: "example.com:8443",
      forwardedHost: "example.com:8443",
      forwardedProto: "https",
      forwardedPort: "8443",
    });
  });

  test("fetch handler preserves upstream redirects", async () => {
    const handler = createNextjsFetchHandler(path.join(rootDir, "server.mjs"), {
      unstable_testing: {
        ensureServer: async () => 4020,
        fetchImplementation: async () =>
          new Response(null, {
            status: 302,
            headers: {
              location: "/target",
            },
          }),
      },
    });
    const response = await handler(new Request("https://example.com/redirect"), {});

    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe("/target");
  });
});
