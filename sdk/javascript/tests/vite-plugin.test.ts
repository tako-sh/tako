import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import { tako } from "../src/vite";
import { expectAsyncToThrow } from "./assertions";

let rootDir = "";
let originalPortEnv: string | undefined;

async function readText(relPath: string): Promise<string> {
  return await readFile(path.join(rootDir, relPath), "utf8");
}

describe("tako Vite entry plugin", () => {
  beforeEach(async () => {
    originalPortEnv = process.env.PORT;
    delete process.env.PORT;
    rootDir = await mkdtemp(path.join(tmpdir(), "tako-vite-plugin-"));
  });

  afterEach(async () => {
    if (originalPortEnv === undefined) {
      delete process.env.PORT;
    } else {
      process.env.PORT = originalPortEnv;
    }
    if (rootDir) {
      await rm(rootDir, { recursive: true, force: true });
    }
  });

  test("writes wrapped server entry for a single entry chunk", async () => {
    await mkdir(path.join(rootDir, "dist"), { recursive: true });

    const plugin = tako();
    plugin.configResolved?.({
      root: rootDir,
      build: { outDir: "dist" },
    });
    plugin.generateBundle?.(
      {},
      {
        "server/index.mjs": {
          type: "chunk",
          fileName: "server/index.mjs",
          isEntry: true,
        },
      },
    );
    await plugin.closeBundle?.();

    const wrapper = await readText("dist/tako-entry.mjs");
    expect(wrapper).toContain('import entryModule, * as entryNamespace from "./server/index.mjs";');
    expect(wrapper).toContain("const fetchHandler");
    expect(wrapper).toContain("entryModule.fetch");
    expect(wrapper).toContain(
      "default fetch function, a default object with fetch, or a named fetch export",
    );
    expect(wrapper).toContain("export default async function");
    expect(wrapper).toContain("handleTakoEndpoint");
    expect(wrapper).toContain("fetchHandler(request)");
  });

  test("externalizes tako.sh from SSR transform", () => {
    const plugin = tako();
    const result = plugin.config?.({}, { command: "build" });
    expect(result).toMatchObject({ ssr: { external: ["tako.sh", "tako.sh/internal"] } });
  });

  test("binds to 127.0.0.1 with .test hosts in serve mode", () => {
    const plugin = tako();
    expect(plugin.config?.({}, { command: "serve" })).toMatchObject({
      server: {
        allowedHosts: [".test", ".tako.test"],
        host: "127.0.0.1",
      },
    });
  });

  test("merges user allowedHosts in serve mode", () => {
    const plugin = tako();
    expect(
      plugin.config?.({ server: { allowedHosts: ["localhost"] } }, { command: "serve" }),
    ).toMatchObject({
      server: { allowedHosts: ["localhost", ".test", ".tako.test"] },
    });
  });

  test("does not set server config in build mode", () => {
    const plugin = tako();
    const result = plugin.config?.({}, { command: "build" });
    expect(result).not.toHaveProperty("server");
  });

  test("installs a JSON customLogger when ENV=development", () => {
    const original = process.env.ENV;
    process.env.ENV = "development";
    try {
      const plugin = tako();
      const result = plugin.config?.({}, { command: "serve" }) as {
        customLogger?: { info?: unknown; warn?: unknown; error?: unknown };
      };
      expect(result.customLogger).toBeDefined();
      expect(typeof result.customLogger?.info).toBe("function");
      expect(typeof result.customLogger?.warn).toBe("function");
      expect(typeof result.customLogger?.error).toBe("function");
    } finally {
      if (original === undefined) delete process.env.ENV;
      else process.env.ENV = original;
    }
  });

  test("does not install customLogger outside development", () => {
    const original = process.env.ENV;
    process.env.ENV = "production";
    try {
      const plugin = tako();
      const result = plugin.config?.({}, { command: "serve" });
      expect(result).not.toHaveProperty("customLogger");
    } finally {
      if (original === undefined) delete process.env.ENV;
      else process.env.ENV = original;
    }
  });

  test("configureServer registers listening handler that reads bound port", () => {
    const plugin = tako();
    plugin.config?.({}, { command: "serve" });

    const listeners: (() => void)[] = [];
    const mockHttpServer = {
      once(_event: string, cb: () => void) {
        listeners.push(cb);
      },
      address() {
        return { address: "127.0.0.1", family: "IPv4", port: 54321 };
      },
    };

    plugin.configureServer?.({
      httpServer: mockHttpServer,
      middlewares: { use: () => {} },
    } as never);

    expect(listeners).toHaveLength(1);
    // Firing the listener should not throw (writeViaInheritedFd catches fd errors silently)
    expect(() => listeners[0]!()).not.toThrow();
  });

  test("configureServer handles null httpServer gracefully", () => {
    const plugin = tako();
    plugin.config?.({}, { command: "serve" });
    expect(() =>
      plugin.configureServer?.({ httpServer: null, middlewares: { use: () => {} } } as never),
    ).not.toThrow();
  });

  test("configureServer installs a middleware that handles tako.internal requests", async () => {
    const plugin = tako();
    plugin.config?.({}, { command: "serve" });

    const installedMiddlewares: ((req: unknown, res: unknown, next: () => void) => void)[] = [];
    const mockServer = {
      httpServer: null,
      middlewares: {
        use(fn: (req: unknown, res: unknown, next: () => void) => void) {
          installedMiddlewares.push(fn);
        },
      },
    };

    plugin.configureServer?.(mockServer as never);
    expect(installedMiddlewares).toHaveLength(1);
  });

  test("prefers entry paths under server when multiple entry chunks exist", async () => {
    const plugin = tako();
    plugin.configResolved?.({
      root: rootDir,
      build: { outDir: "dist" },
    });
    plugin.generateBundle?.(
      {},
      {
        "client/index.js": {
          type: "chunk",
          fileName: "client/index.js",
          isEntry: true,
        },
        "server/index.mjs": {
          type: "chunk",
          fileName: "server/index.mjs",
          isEntry: true,
        },
      },
    );
    await plugin.closeBundle?.();

    const wrapper = await readText("dist/tako-entry.mjs");
    expect(wrapper).toContain('import entryModule, * as entryNamespace from "./server/index.mjs";');
  });

  test("fails clearly when multiple entries are ambiguous", async () => {
    const plugin = tako();
    plugin.configResolved?.({
      root: rootDir,
      build: { outDir: "dist" },
    });
    plugin.generateBundle?.(
      {},
      {
        "entry-a.js": { type: "chunk", fileName: "entry-a.js", isEntry: true },
        "entry-b.js": { type: "chunk", fileName: "entry-b.js", isEntry: true },
      },
    );

    await expectAsyncToThrow(
      () => plugin.closeBundle?.(),
      "Could not choose a single server entry chunk",
    );
  });

  test("fails clearly when no entry chunks exist", async () => {
    const plugin = tako();
    plugin.configResolved?.({
      root: rootDir,
      build: { outDir: "dist" },
    });
    plugin.generateBundle?.(
      {},
      {
        "chunk.js": {
          type: "chunk",
          fileName: "chunk.js",
          isEntry: false,
        },
      },
    );

    await expectAsyncToThrow(() => plugin.closeBundle?.(), "Could not detect server entry chunk");
  });

  test("fails when closeBundle runs before configResolved", async () => {
    const plugin = tako();
    await expectAsyncToThrow(
      () => plugin.closeBundle?.(),
      "tako was not initialized by Vite configResolved hook.",
    );
  });

  test("writes wrapped entry inside the configured outDir", async () => {
    await mkdir(path.join(rootDir, "dist/server"), { recursive: true });

    const plugin = tako();
    plugin.configResolved?.({
      root: rootDir,
      build: { outDir: "dist/server" },
    });
    plugin.generateBundle?.(
      {},
      {
        "server.js": {
          type: "chunk",
          fileName: "server.js",
          isEntry: true,
        },
      },
    );
    await plugin.closeBundle?.();

    const wrapper = await readText("dist/server/tako-entry.mjs");
    expect(wrapper).toContain('import entryModule, * as entryNamespace from "./server.js";');
  });

  test("does not write deploy metadata files", async () => {
    await mkdir(path.join(rootDir, "dist"), { recursive: true });

    const plugin = tako();
    plugin.configResolved?.({
      root: rootDir,
      build: { outDir: "dist" },
    });
    plugin.generateBundle?.(
      {},
      {
        "server/index.mjs": {
          type: "chunk",
          fileName: "server/index.mjs",
          isEntry: true,
        },
      },
    );
    await plugin.closeBundle?.();

    await expectAsyncToThrow(() => readText(".tako/build.json"));
    await expectAsyncToThrow(() => readText("dist/.tako-vite.json"));
  });
});
