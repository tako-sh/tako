import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import { createConnection, createServer } from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";

import type { FetchHandler, ReadyableFetchHandler } from "../types";
import type { ManagedNextjsServer, ManagedNextjsStartup, NextjsFetchHandlerOptions } from "./types";

const DEFAULT_HOSTNAME = "127.0.0.1";
const DEFAULT_STARTUP_TIMEOUT_MS = 30_000;

const managedServers = new Map<string, ManagedNextjsServer>();
let cleanupHandlersRegistered = false;
let shutdownPromise: Promise<void> | null = null;

/**
 * Create a fetch handler that starts and proxies to a Next.js standalone server.
 *
 * The returned handler implements Tako's fetch-handler shape and exposes a
 * `ready()` hook so Tako can wait for Next before routing traffic.
 *
 * @param serverEntrypoint - Path or URL to the Next standalone `server.js`.
 * @param options - Startup and proxy options.
 * @defaultValue options = {}
 */
export function createNextjsFetchHandler(
  serverEntrypoint: string | URL,
  options: NextjsFetchHandlerOptions = {},
): FetchHandler {
  const serverPath = normalizeFileReference(serverEntrypoint);
  const hostname = options.hostname ?? DEFAULT_HOSTNAME;
  const fetchImplementation = options.unstable_testing?.fetchImplementation ?? fetch;

  const handler: ReadyableFetchHandler = async (request) => {
    const server = options.unstable_testing?.ensureServer
      ? { hostname, port: await options.unstable_testing.ensureServer() }
      : await ensureManagedNextjsServer(serverPath, options);
    const upstreamUrl = new URL(request.url);
    const originalHost = request.headers.get("host") ?? upstreamUrl.host;
    const originalPort = originalRequestPort(request, originalHost);

    upstreamUrl.protocol = "http:";
    upstreamUrl.hostname = server.hostname;
    upstreamUrl.port = String(server.port);

    const headers = new Headers(request.headers);
    headers.set("host", originalHost);
    headers.set("x-forwarded-host", originalHost);
    headers.set("x-forwarded-proto", new URL(request.url).protocol.replace(/:$/, ""));
    if (!headers.has("x-forwarded-port") && originalPort) {
      headers.set("x-forwarded-port", originalPort);
    }

    const init: RequestInit & { duplex?: "half" } = {
      method: request.method,
      headers,
      redirect: "manual",
    };
    if (request.body && request.method !== "GET" && request.method !== "HEAD") {
      init.body = request.body;
      init.duplex = "half";
    }

    return await fetchImplementation(upstreamUrl, init);
  };

  handler.ready = async () => {
    if (options.unstable_testing?.ensureServer) {
      await options.unstable_testing.ensureServer();
      return;
    }
    await ensureManagedNextjsServer(serverPath, options);
  };

  return handler;
}

/**
 * Stop every Next.js child process managed by this module.
 *
 * @internal Used by tests and process shutdown hooks.
 */
export async function shutdownManagedNextjsServers(): Promise<void> {
  if (shutdownPromise) return await shutdownPromise;
  shutdownPromise = (async () => {
    const results = await Promise.allSettled(
      [...managedServers.entries()].map(async ([key, managed]) => {
        const startup = managed.startup;
        if (startup) {
          startup.abort.abort(new Error("Managed Next.js server is shutting down."));
          await Promise.all([
            startup.child ? stopChildProcess(key, startup.child) : Promise.resolve(),
            startup.ready.catch(() => {}),
          ]);
        }
        managedServers.delete(key);
      }),
    );
    const errors = results.flatMap((result) =>
      result.status === "rejected" ? [result.reason] : [],
    );
    if (errors.length > 0)
      throw new AggregateError(errors, "Failed to stop managed Next.js servers.");
  })().finally(() => {
    shutdownPromise = null;
  });
  return await shutdownPromise;
}

function getManagedNextjsServer(
  serverPath: string,
  options: NextjsFetchHandlerOptions,
): ManagedNextjsServer {
  const serverKey = managedNextjsServerKey(serverPath, options);
  let managed = managedServers.get(serverKey);
  if (!managed) {
    managed = {
      startup: null,
      argv: options.argv ?? [],
      cwd: normalizeExecutionCwd(serverPath, options.cwd),
      hostname: options.hostname ?? DEFAULT_HOSTNAME,
      startupTimeoutMs: options.startupTimeoutMs ?? DEFAULT_STARTUP_TIMEOUT_MS,
    };
    managedServers.set(serverKey, managed);
  }
  return managed;
}

async function ensureManagedNextjsServer(
  serverPath: string,
  options: NextjsFetchHandlerOptions,
): Promise<{ hostname: string; port: number }> {
  while (shutdownPromise) await shutdownPromise;
  const managed = getManagedNextjsServer(serverPath, options);
  if (!managed.startup) {
    const { promise, resolve, reject } = Promise.withResolvers<number>();
    const startup: ManagedNextjsStartup = {
      child: null,
      ready: promise,
      abort: new AbortController(),
    };
    managed.startup = startup;
    void startManagedNextjsServer(serverPath, managed, startup).then(resolve, reject);
  }

  return {
    hostname: managed.hostname,
    port: await managed.startup.ready,
  };
}

async function startManagedNextjsServer(
  serverPath: string,
  managed: ManagedNextjsServer,
  startup: ManagedNextjsStartup,
): Promise<number> {
  const signal = startup.abort.signal;
  const clear = () => {
    if (managed.startup === startup) managed.startup = null;
  };
  try {
    await ensureFileExists(
      serverPath,
      `Next.js server entry '${serverPath}' was not found. Run next build before deploy.`,
    );
    signal.throwIfAborted();
    const port = await reservePort(managed.hostname);
    signal.throwIfAborted();
    const child = spawn(process.execPath, [serverPath, ...managed.argv], {
      cwd: managed.cwd,
      env: {
        ...process.env,
        HOST: managed.hostname,
        PORT: String(port),
      },
      stdio: "inherit",
    });
    startup.child = child;
    registerCleanupHandlers();
    const exited = new Promise<never>((_, reject) => {
      child.once("exit", () => {
        clear();
        reject(new Error(`Next.js server '${serverPath}' exited before it became ready.`));
      });
      child.once("error", reject);
    });
    await Promise.race([
      waitForServerReady(
        serverPath,
        child,
        managed.hostname,
        port,
        managed.startupTimeoutMs,
        signal,
      ),
      exited,
    ]);
    signal.throwIfAborted();
    return port;
  } catch (error) {
    startup.abort.abort(error);
    if (startup.child) await stopChildProcess(serverPath, startup.child);
    clear();
    throw error;
  }
}

function registerCleanupHandlers(): void {
  if (cleanupHandlersRegistered) {
    return;
  }
  cleanupHandlersRegistered = true;

  const shutdown = () => {
    void shutdownManagedNextjsServers();
  };
  process.once("exit", shutdown);
  process.once("SIGINT", () => {
    void shutdownManagedNextjsServers().finally(() => process.exit(130));
  });
  process.once("SIGTERM", () => {
    void shutdownManagedNextjsServers().finally(() => process.exit(143));
  });
}

async function waitForServerReady(
  serverPath: string,
  child: NonNullable<ManagedNextjsStartup["child"]>,
  hostname: string,
  port: number,
  timeoutMs: number,
  signal: AbortSignal,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    signal.throwIfAborted();
    if (childExited(child)) {
      throw new Error(`Next.js server '${serverPath}' exited before it became ready.`);
    }
    if (await canConnect(hostname, port)) {
      return;
    }
    await sleep(50);
  }

  signal.throwIfAborted();
  throw new Error(`Timed out waiting for Next.js server '${serverPath}' to start.`);
}

async function stopChildProcess(
  serverPath: string,
  child: NonNullable<ManagedNextjsStartup["child"]>,
): Promise<void> {
  if (childExited(child) || child.pid === undefined) return;
  child.kill("SIGTERM");
  if (await waitForChildExit(child, 5_000)) return;
  child.kill("SIGKILL");
  if (await waitForChildExit(child, 5_000)) return;
  throw new Error(`Timed out shutting down managed Next.js server '${serverPath}'.`);
}

function childExited(child: NonNullable<ManagedNextjsStartup["child"]>): boolean {
  return child.exitCode !== null || child.signalCode !== null;
}

async function waitForChildExit(
  child: NonNullable<ManagedNextjsStartup["child"]>,
  timeoutMs: number,
): Promise<boolean> {
  if (childExited(child)) return true;
  return await new Promise<boolean>((resolve) => {
    const finish = (exited: boolean) => {
      clearTimeout(timer);
      child.removeListener("exit", onExit);
      resolve(exited);
    };
    const onExit = () => finish(true);
    const timer = setTimeout(() => finish(false), timeoutMs);
    child.once("exit", onExit);
  });
}

async function reservePort(hostname: string): Promise<number> {
  const server = createServer();
  return await new Promise<number>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, hostname, () => {
      const address = server.address();
      if (!address || typeof address === "string") {
        server.close();
        reject(new Error("Failed to reserve an ephemeral port."));
        return;
      }
      const port = address.port;
      server.close((error) => {
        if (error) {
          reject(error);
          return;
        }
        resolve(port);
      });
    });
  });
}

async function canConnect(hostname: string, port: number): Promise<boolean> {
  return await new Promise<boolean>((resolve) => {
    const connection = createConnection({ host: hostname, port });
    connection.once("connect", () => {
      connection.end();
      resolve(true);
    });
    connection.once("error", () => {
      connection.destroy();
      resolve(false);
    });
  });
}

async function ensureFileExists(targetPath: string, message: string): Promise<void> {
  try {
    await access(targetPath);
  } catch {
    throw new Error(message);
  }
}

function managedNextjsServerKey(serverPath: string, options: NextjsFetchHandlerOptions): string {
  return JSON.stringify({
    serverPath,
    argv: options.argv ?? [],
    cwd: options.cwd ? normalizeFileReference(options.cwd) : path.dirname(serverPath),
  });
}

function originalRequestPort(request: Request, originalHost: string): string | null {
  const hostPort = portFromHostHeader(originalHost);
  if (hostPort) {
    return hostPort;
  }

  const requestPort = new URL(request.url).port;
  return requestPort || null;
}

function portFromHostHeader(host: string): string | null {
  const trimmed = host.trim();
  if (!trimmed) {
    return null;
  }
  if (trimmed.startsWith("[")) {
    const closingIndex = trimmed.indexOf("]");
    if (closingIndex === -1) {
      return null;
    }
    const suffix = trimmed.slice(closingIndex + 1);
    if (!suffix.startsWith(":")) {
      return null;
    }
    return suffix.slice(1) || null;
  }

  const lastColonIndex = trimmed.lastIndexOf(":");
  if (lastColonIndex === -1 || trimmed.indexOf(":") !== lastColonIndex) {
    return null;
  }
  return trimmed.slice(lastColonIndex + 1) || null;
}

function normalizeExecutionCwd(serverPath: string, cwd: string | URL | undefined): string {
  if (!cwd) {
    return path.dirname(serverPath);
  }
  return normalizeFileReference(cwd);
}

function normalizeFileReference(value: string | URL): string {
  if (value instanceof URL) {
    return fileURLToPath(value);
  }
  return path.resolve(value);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
