import { mkdir, writeFile } from "node:fs/promises";
import type { IncomingMessage, ServerResponse } from "node:http";
import path from "node:path";
import type { Plugin, ResolvedConfig, UserConfig } from "vite";
import { bootstrapChannels } from "./channels/bootstrap";
import type { ChannelRegistry } from "./channels";
import { createLogger } from "./logger";
import { TAKO_INTERNAL_HOST_SUFFIX, handleTakoEndpoint } from "./tako/endpoints";
import { initServerRuntime } from "./tako/init";
import { installConsoleBridge } from "./tako/console-bridge";
import { writeViaInheritedFd } from "./tako/readiness";
import { installStdioBridge } from "./tako/stdio-bridge";

interface ViteEntryChunkLike {
  type: "chunk";
  fileName: string;
  isEntry: boolean;
}

const WRAPPED_ENTRY_FILE = "tako-entry.mjs";
const TAKO_DEV_ALLOWED_HOSTS_ENV = "TAKO_DEV_ALLOWED_HOSTS";

function statusAppName(fallback: string): string {
  const [appName = ""] = (process.env["TAKO_APP_NAME"] || fallback).split("/");
  return appName || fallback;
}

function toPosixPath(filePath: string): string {
  return filePath.replaceAll("\\", "/");
}

function toRelativeImportSpecifier(filePath: string): string {
  const normalized = toPosixPath(filePath);
  if (normalized.startsWith("./") || normalized.startsWith("../")) {
    return normalized;
  }
  return `./${normalized}`;
}

function renderWrappedEntrySource(compiledMain: string): string {
  const importSpecifier = toRelativeImportSpecifier(compiledMain);
  return `import entryModule, * as entryNamespace from ${JSON.stringify(importSpecifier)};
import { handleTakoEndpoint, normalizeFetchResponse } from "tako.sh/internal";

const fetchHandler =
  typeof entryModule === "function"
    ? entryModule
    : entryModule && typeof entryModule.fetch === "function"
      ? entryModule.fetch.bind(entryModule)
      : typeof entryNamespace.fetch === "function"
        ? entryNamespace.fetch
        : null;

if (!fetchHandler) {
  throw new Error(
    "Invalid server entry: export a default fetch function, a default object with fetch, or a named fetch export.",
  );
}

export default async function(request) {
  const [appName = ""] = (process.env.TAKO_APP_NAME ?? "app").split("/");
  const takoResponse = await handleTakoEndpoint(request, {
    status: "healthy",
    app: appName || "app",
    version: process.env.TAKO_BUILD ?? "unknown",
    instance_id: process.env.TAKO_INSTANCE_ID ?? "unknown",
    pid: process.pid,
    uptime_seconds: 0,
  });
  if (takoResponse) return takoResponse;
  return normalizeFetchResponse(await fetchHandler(request));
};
`;
}

function pickCompiledMain(entries: string[]): string {
  if (entries.length === 0) {
    throw new Error(
      "Could not detect server entry chunk in Vite build output. Ensure your SSR/server build emits an entry chunk.",
    );
  }

  if (entries.length === 1) {
    return entries[0]!;
  }

  const serverEntries = entries.filter((entry) =>
    entry
      .split("/")
      .map((segment) => segment.toLowerCase())
      .includes("server"),
  );

  if (serverEntries.length === 1) {
    return serverEntries[0]!;
  }

  throw new Error(
    `Could not choose a single server entry chunk from Vite output. Found: ${entries.join(", ")}. Configure your build to emit one server entry chunk.`,
  );
}

function nodeRequestToFetch(req: IncomingMessage): Promise<Request> {
  const host = req.headers.host ?? "localhost";
  const url = `http://${host}${req.url ?? "/"}`;
  const headers = new Headers();
  for (const [key, val] of Object.entries(req.headers)) {
    if (val === undefined) continue;
    if (Array.isArray(val)) {
      for (const v of val) headers.append(key, v);
    } else {
      headers.set(key, val);
    }
  }
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      const init: RequestInit = {
        method: req.method ?? "GET",
        headers,
      };
      if (chunks.length > 0) {
        init.body = Buffer.concat(chunks) as unknown as BodyInit;
      }
      resolve(new Request(url, init));
    });
    req.on("error", reject);
  });
}

async function sendFetchResponse(res: ServerResponse, response: Response): Promise<void> {
  res.statusCode = response.status;
  for (const [key, val] of response.headers.entries()) {
    res.setHeader(key, val);
  }
  res.end(Buffer.from(await response.arrayBuffer()));
}

function mergeServeAllowedHosts(
  existing: unknown,
  extraHosts = devAllowedHostsFromEnv(),
): true | string[] {
  if (existing === true) {
    return true;
  }

  const merged = Array.isArray(existing)
    ? existing.filter((host): host is string => typeof host === "string")
    : [];

  for (const host of [".test", ".tako.test", ...extraHosts]) {
    if (!merged.includes(host)) {
      merged.push(host);
    }
  }

  return merged;
}

function devAllowedHostsFromEnv(): string[] {
  const raw = process.env[TAKO_DEV_ALLOWED_HOSTS_ENV];
  if (!raw) {
    return [];
  }
  return raw
    .split(",")
    .map((host) => host.trim())
    .filter(Boolean);
}

function isViteEntryChunk(chunk: unknown): chunk is ViteEntryChunkLike {
  if (!chunk || typeof chunk !== "object") {
    return false;
  }

  const maybeChunk = chunk as Partial<ViteEntryChunkLike>;
  return (
    maybeChunk.type === "chunk" &&
    maybeChunk.isEntry === true &&
    typeof maybeChunk.fileName === "string"
  );
}

/**
 * Vite plugin that wires a project up to the Tako build/dev pipeline.
 *
 * Responsibilities:
 * - Marks `tako.sh` as SSR-external so Vite doesn't try to bundle server-only
 *   modules (secrets, workflow RPC, etc.).
 * - In dev, swaps Vite's default logger for structured JSON lines so the
 *   tako dev server can render them alongside other subprocess logs.
 * - Under `tako dev`, reports the dev server's bound port back to the parent
 *   over fd 4 and adds configured dev route hosts to `server.allowedHosts`.
 * - On build, records the entry chunk filenames so the Tako runtime can find
 *   the generated entrypoint.
 *
 * Add to `vite.config.ts` alongside any framework plugin:
 *
 * @example
 * ```typescript
 * import { defineConfig } from "vite";
 * import { tako } from "tako.sh/vite";
 *
 * export default defineConfig({ plugins: [tako()] });
 * ```
 *
 * @returns A Vite {@link Plugin} instance.
 */
export function tako(): Plugin {
  let resolvedConfig: ResolvedConfig | null = null;
  let entryChunks: string[] = [];
  let sawBundleGeneration = false;
  let activeCommand: "build" | "serve" | null = null;

  return {
    name: "tako-vite-entry",
    config(userConfig, env) {
      activeCommand = env.command;

      const config: UserConfig = {};

      // Exclude the SDK from Vite's SSR transform — it's a server-side
      // dependency with runtime dynamic imports Vite can't statically analyze.
      config.ssr = { external: ["tako.sh", "tako.sh/internal"] };

      // Under the tako dev server, emit structured JSON log lines so the
      // parent process can render Vite output alongside other subprocess logs.
      if (process.env["ENV"] === "development") {
        installStdioBridge("app");
        installConsoleBridge("app");
        config.customLogger = createLogger("vite").toViteLogger();
      }

      if (activeCommand === "serve") {
        // Let Vite pick its own port — the configureServer hook reports
        // the actual bound port to Tako via fd 4.
        config.server = {
          allowedHosts: mergeServeAllowedHosts(userConfig.server?.allowedHosts),
          host: "127.0.0.1",
        };
      }

      return config;
    },
    configResolved(config) {
      resolvedConfig = config;
    },
    configureServer(server) {
      // Wire up the same server-runtime install used by the production
      // entrypoint — so user server fns can `signal()`, `.enqueue()`, and
      // publish to channels during `tako dev` without boot-time setup.
      initServerRuntime();

      // Discover channel definitions from `<appRoot>/channels/` once at startup.
      // The registry feeds the internal channel-auth/dispatch endpoints.
      let channelsPromise: Promise<ChannelRegistry> | null = null;
      const getChannels = (): Promise<ChannelRegistry> => {
        if (!channelsPromise) {
          channelsPromise = bootstrapChannels({ appDir: process.cwd() }).then((r) => r.registry);
        }
        return channelsPromise;
      };

      server.middlewares.use((req: IncomingMessage, res: ServerResponse, next: () => void) => {
        const host = (req.headers.host ?? "").split(":")[0] ?? "";
        if (!host.endsWith(TAKO_INTERNAL_HOST_SUFFIX)) {
          next();
          return;
        }
        Promise.all([nodeRequestToFetch(req), getChannels()])
          .then(([fetchReq, channels]) =>
            handleTakoEndpoint(
              fetchReq,
              {
                status: "healthy",
                app: statusAppName("dev"),
                version: process.env["TAKO_BUILD"] ?? "dev",
                instance_id: process.env["TAKO_INSTANCE_ID"] ?? "dev",
                pid: process.pid,
                uptime_seconds: 0,
              },
              channels,
            ),
          )
          .then((response) => {
            if (response) return sendFetchResponse(res, response);
            next();
            return;
          })
          .catch(() => next());
      });

      server.httpServer?.once("listening", () => {
        const addr = server.httpServer?.address();
        if (addr && typeof addr === "object") {
          writeViaInheritedFd(4, addr.port);
        }
      });
    },
    generateBundle(_options, bundle) {
      sawBundleGeneration = true;
      entryChunks = Object.values(bundle)
        .filter(isViteEntryChunk)
        .map((chunk) => chunk.fileName)
        .sort();
    },
    async closeBundle() {
      if (activeCommand === "serve") {
        return;
      }
      if (!resolvedConfig) {
        throw new Error("tako was not initialized by Vite configResolved hook.");
      }
      if (!sawBundleGeneration) {
        return;
      }

      const outDirAbs = path.isAbsolute(resolvedConfig.build.outDir)
        ? path.normalize(resolvedConfig.build.outDir)
        : path.resolve(resolvedConfig.root, resolvedConfig.build.outDir);
      const compiledMain = pickCompiledMain(entryChunks);
      const wrappedEntrySource = renderWrappedEntrySource(compiledMain);
      const wrappedEntryPath = path.resolve(outDirAbs, WRAPPED_ENTRY_FILE);

      await mkdir(path.dirname(wrappedEntryPath), { recursive: true });
      await writeFile(wrappedEntryPath, wrappedEntrySource, "utf8");
    },
  };
}
