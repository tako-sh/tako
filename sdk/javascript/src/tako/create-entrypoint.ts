/**
 * Creates a Tako entrypoint for any JS runtime.
 *
 * Each runtime-specific entrypoint calls `createEntrypoint` which handles
 * the runtime-agnostic boot flow.
 *
 * CLI args (appended by tako-server):
 *   <main> --instance <id>
 */

import { isAbsolute, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { handleTakoEndpoint } from "./endpoints";
import { normalizeFetchResponse } from "./fetch-response";
import { writeViaInheritedFd } from "./readiness";
import { bootstrapChannels } from "../channels/bootstrap";
import type { ChannelRegistry } from "../channels";
import { initServerRuntime } from "./init";
import type { FetchFunction, ReadyableFetchHandler, TakoStatus } from "../types";

/** Exported for tests and for runtime entrypoints that want the default impl. */
export const signalReadyPortOnFd = writeViaInheritedFd;

interface ParsedArgs {
  main: string;
  instance: string;
}

function parseArgs(argv: string[]): ParsedArgs {
  const args = argv.slice(2);
  let main = "";
  let instance = "unknown";

  for (let i = 0; i < args.length; i++) {
    switch (args[i]) {
      case "--instance":
        instance = args[++i] ?? "unknown";
        break;
      default:
        if (!main && !args[i]?.startsWith("--")) {
          main = args[i] ?? "";
        }
        break;
    }
  }

  return { main, instance };
}

function statusAppName(): string {
  const [appName = ""] = (process.env["TAKO_APP_NAME"] || "app").split("/");
  return appName || "app";
}

export function createEntrypoint() {
  const signalReadyPort = (port: number): void => writeViaInheritedFd(4, port);

  const parsed = parseArgs(process.argv);
  const port = parseInt(process.env["PORT"] || "3000", 10);
  const host = process.env["HOST"] || "127.0.0.1";

  const startedAt = Date.now();
  let currentStatus: TakoStatus["status"] = "starting";

  function getStatus(): TakoStatus {
    return {
      status: currentStatus,
      app: statusAppName(),
      version: process.env["TAKO_BUILD"] || "unknown",
      instance_id: parsed.instance,
      pid: process.pid,
      uptime_seconds: Math.floor((Date.now() - startedAt) / 1000),
    };
  }

  function setDraining(): void {
    currentStatus = "draining";
  }

  async function run(
    startServer: (
      handleRequest: (request: Request) => Promise<Response>,
    ) => number | void | Promise<number | void>,
  ): Promise<void> {
    if (!parsed.main) {
      console.error("Usage: <runtime> entrypoint <main> [--instance <id>]");
      process.exit(1);
    }

    initServerRuntime();

    let channels: ChannelRegistry;
    try {
      const result = await bootstrapChannels({ appDir: process.cwd() });
      channels = result.registry;
    } catch (err) {
      console.error("Failed to load channels/ directory:", err);
      process.exit(1);
    }

    let userFetch: FetchFunction;
    let userReady: (() => void | Promise<void>) | null = null;
    try {
      // `parsed.main` is a filesystem path from the spawner's launch args,
      // relative to the app cwd. Convert to a file:// URL so dynamic
      // `import()` resolves it against the app — not the SDK module URL,
      // which lives under `node_modules/tako.sh/dist/`.
      const mainPath = isAbsolute(parsed.main) ? parsed.main : resolve(process.cwd(), parsed.main);
      const mainUrl = pathToFileURL(mainPath).href;
      const module = (await import(/* @vite-ignore */ mainUrl)) as { default?: unknown };
      const defaultExport = module.default;
      if (typeof defaultExport === "function") {
        const readyable = defaultExport as ReadyableFetchHandler;
        userFetch = readyable;
        if (typeof readyable.ready === "function") {
          userReady = () => readyable.ready?.();
        }
      } else if (
        defaultExport &&
        typeof defaultExport === "object" &&
        typeof (defaultExport as { fetch?: unknown }).fetch === "function"
      ) {
        const obj = defaultExport as { fetch: FetchFunction; ready?: () => void | Promise<void> };
        userFetch = obj.fetch;
        if (typeof obj.ready === "function") {
          const ready = obj.ready;
          userReady = () => ready();
        }
      } else {
        throw new Error("App must export a default fetch function or { fetch } object.");
      }
    } catch (err) {
      console.error(`Failed to import app from ${parsed.main}:`, err);
      process.exit(1);
    }

    if (userReady) {
      try {
        await userReady();
      } catch (err) {
        console.error(`Failed to initialize app readiness from ${parsed.main}:`, err);
        process.exit(1);
      }
    }

    const env: Record<string, string> = {};
    for (const [key, value] of Object.entries(process.env)) {
      if (value !== undefined) {
        env[key] = value;
      }
    }

    const handleRequest = async (request: Request): Promise<Response> => {
      const takoResponse = await handleTakoEndpoint(request, getStatus(), channels);
      if (takoResponse) {
        return takoResponse;
      }

      try {
        return normalizeFetchResponse(await userFetch(request, env));
      } catch (err) {
        console.error("Error in user fetch handler:", err);
        return new Response(JSON.stringify({ error: "Internal Server Error" }), {
          status: 500,
          headers: { "Content-Type": "application/json" },
        });
      }
    };

    const actualPort = await startServer(handleRequest);
    currentStatus = "healthy";
    if (actualPort != null) {
      signalReadyPort(actualPort);
    }
  }

  return { run, host, port, setDraining };
}
