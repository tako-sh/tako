/**
 * Shared Tako internal unix-socket RPC client.
 *
 * Server-side SDK code (app fetch handlers, workflow bodies, cron ticks)
 * reaches `tako-server` via a single unix socket. Workflow RPCs and
 * server-side channel `.publish()` both land here — no HTTPS, no auth, same
 * trust boundary as the hosting process.
 *
 * Env vars set by the server when spawning an instance or worker:
 *   TAKO_INTERNAL_SOCKET — path to the shared unix socket
 *   TAKO_APP_NAME        — app name used on every command payload
 */

import { createConnection } from "node:net";
import type { Socket } from "node:net";
import { setChannelSocketPublisher } from "../channels";
import { createLogger } from "../logger";
import type { ChannelMessage, ChannelPublishInput } from "../types";

export const INTERNAL_SOCKET_ENV = "TAKO_INTERNAL_SOCKET";
export const APP_NAME_ENV = "TAKO_APP_NAME";

export { TakoError, type TakoErrorCode } from "./error";
import { TakoError, type TakoErrorCode } from "./error";

interface RpcResponse {
  status: "ok" | "error";
  data?: unknown;
  message?: string;
}

const logger = createLogger("sdk.rpc");
const DEFAULT_INTERNAL_SOCKET_POOL_SIZE = 64;
const RPC_TIMEOUT_MS = 30_000;

type ResolveResponse = (value: RpcResponse) => void;
type RejectResponse = (reason: unknown) => void;

interface PendingRpc {
  cmd: unknown;
  resolve: ResolveResponse;
  reject: RejectResponse;
}

const internalSocketPools = new Map<string, InternalSocketPool>();

/**
 * Log the raw failure and return a sanitized `TakoError`. Callers throw the
 * returned value; the original error stays on `.cause` for local debugging
 * but never flows to an end user via `.message`. The stable `code` field
 * lets app code branch by failure class.
 */
function wrapSocketError(code: TakoErrorCode, cause: unknown): TakoError {
  logger.error("rpc failed", { code, error: cause });
  return new TakoError(code, "Internal Server Error", { cause });
}

/**
 * Look up the `(socketPath, appName)` pair from env. Returns `null` when
 * either var is missing — callers decide whether to fall back (HTTPS for
 * channels) or throw (workflow RPC).
 */
export function internalSocketFromEnv(): { socketPath: string; app: string } | null {
  const envObj = typeof process !== "undefined" ? process.env : undefined;
  if (!envObj) return null;
  const socketPath = envObj[INTERNAL_SOCKET_ENV];
  const app = envObj[APP_NAME_ENV];
  if (!socketPath || !app) return null;
  return { socketPath, app };
}

/**
 * Validate the Tako runtime env contract: `TAKO_INTERNAL_SOCKET` and
 * `TAKO_APP_NAME` must be set together or not at all.
 *
 * Called once at SDK init so a misconfigured spawn (one var set, the other
 * missing) crashes the process on boot instead of hiding until the first
 * workflow `.enqueue()` or channel `.publish()`. Both spawners
 * (`tako-server`, `tako-dev-server`) always set the pair, so a half-set
 * state is a platform bug worth failing loud.
 */
export function assertInternalSocketEnvConsistency(): void {
  const envObj = typeof process !== "undefined" ? process.env : undefined;
  if (!envObj) return;
  const hasSocket = Boolean(envObj[INTERNAL_SOCKET_ENV]);
  const hasApp = Boolean(envObj[APP_NAME_ENV]);
  if (hasSocket === hasApp) return;
  const missing = hasSocket ? APP_NAME_ENV : INTERNAL_SOCKET_ENV;
  const present = hasSocket ? INTERNAL_SOCKET_ENV : APP_NAME_ENV;
  throw new Error(
    `Tako SDK: ${present} is set but ${missing} is missing. ` +
      `Both env vars must be set together (or neither — when running ` +
      `outside a Tako-managed process). This usually means the spawner ` +
      `forgot to inject the full Tako runtime contract.`,
  );
}

/**
 * Install a channel publisher that routes `Channel.publish()` calls through
 * the Tako internal socket when `TAKO_INTERNAL_SOCKET` + `TAKO_APP_NAME` are
 * set. Called from the server and worker bootstraps so app/workflow code can
 * publish without an HTTPS round-trip back through the proxy.
 *
 * Returns `true` when a publisher was installed, `false` when the env is
 * missing (e.g. running outside a Tako-managed process).
 */
export function installChannelSocketPublisherFromEnv(): boolean {
  const env = internalSocketFromEnv();
  if (!env) return false;
  const { socketPath, app } = env;
  setChannelSocketPublisher(async <T>(channel: string, message: ChannelPublishInput<T>) => {
    const result = await callInternal(socketPath, {
      command: "channel_publish",
      app,
      channel,
      payload: message,
    });
    return result as ChannelMessage<T>;
  });
  return true;
}

/** Send a single JSONL command and resolve to `data` (or throw on error). */
export async function callInternal(socketPath: string, cmd: unknown): Promise<unknown> {
  const resp = await internalSocketPool(socketPath).call(cmd);
  if (resp.status === "error") {
    logger.error("rpc rejected", { code: "TAKO_RPC_ERROR", message: resp.message });
    throw new TakoError("TAKO_RPC_ERROR", "Internal Server Error", {
      cause: resp.message ? new Error(resp.message) : undefined,
    });
  }
  return resp.data ?? null;
}

function internalSocketPool(socketPath: string): InternalSocketPool {
  const existing = internalSocketPools.get(socketPath);
  if (existing) return existing;
  const created = new InternalSocketPool(socketPath, DEFAULT_INTERNAL_SOCKET_POOL_SIZE);
  internalSocketPools.set(socketPath, created);
  return created;
}

export function closeInternalSocketPoolsForTests(): void {
  for (const pool of internalSocketPools.values()) {
    pool.close();
  }
  internalSocketPools.clear();
}

class InternalSocketPool {
  private readonly idle: RpcConnection[] = [];
  private readonly active = new Set<RpcConnection>();
  private readonly pending: PendingRpc[] = [];

  // JSONL responses do not carry request ids, so each socket lane keeps at
  // most one in-flight RPC. Parallelism comes from bounded lanes; sequential
  // calls reuse idle sockets instead of reconnecting for every publish/enqueue.
  constructor(
    private readonly socketPath: string,
    private readonly maxConnections: number,
  ) {}

  call(cmd: unknown): Promise<RpcResponse> {
    return new Promise<RpcResponse>((resolve, reject) => {
      this.pending.push({ cmd, resolve, reject });
      this.drain();
    });
  }

  close(): void {
    while (this.pending.length > 0) {
      const pending = this.pending.shift();
      pending?.reject(wrapSocketError("TAKO_UNAVAILABLE", new Error("rpc pool closed")));
    }
    for (const conn of this.active) {
      conn.close();
    }
    this.active.clear();
    this.idle.length = 0;
  }

  private drain(): void {
    while (this.pending.length > 0) {
      const conn = this.nextConnection();
      if (!conn) return;

      const request = this.pending.shift();
      if (!request) {
        this.returnConnection(conn);
        return;
      }

      conn
        .send(request.cmd)
        .then(request.resolve, request.reject)
        .finally(() => {
          this.returnConnection(conn);
          this.drain();
        });
    }
  }

  private nextConnection(): RpcConnection | null {
    const idle = this.idle.pop();
    if (idle) return idle;

    if (this.active.size >= this.maxConnections) {
      return null;
    }

    const conn = new RpcConnection(this.socketPath, () => {
      this.active.delete(conn);
      const idx = this.idle.indexOf(conn);
      if (idx !== -1) {
        this.idle.splice(idx, 1);
      }
    });
    this.active.add(conn);
    return conn;
  }

  private returnConnection(conn: RpcConnection): void {
    if (!conn.closed && this.active.has(conn)) {
      this.idle.push(conn);
    }
  }
}

class RpcConnection {
  private readonly socket: Socket;
  private readonly connected: Promise<void>;
  private buf = "";
  private current: PendingRpc | null = null;
  private timeout: ReturnType<typeof setTimeout> | null = null;
  closed = false;

  constructor(
    socketPath: string,
    private readonly onClose: () => void,
  ) {
    this.socket = createConnection(socketPath);
    this.connected = new Promise<void>((resolve, reject) => {
      this.socket.once("connect", resolve);
      this.socket.once("error", reject);
    });

    this.socket.on("data", (chunk) => this.onData(chunk));
    this.socket.on("error", (err) => {
      this.failCurrent("TAKO_UNAVAILABLE", err);
      this.markClosed();
    });
    this.socket.on("end", () => {
      this.failCurrent("TAKO_PROTOCOL", new Error("socket closed without response"));
      this.markClosed();
    });
    this.socket.on("close", () => this.markClosed());
  }

  async send(cmd: unknown): Promise<RpcResponse> {
    if (this.closed) {
      throw wrapSocketError("TAKO_UNAVAILABLE", new Error("socket is closed"));
    }
    if (this.current) {
      throw wrapSocketError("TAKO_PROTOCOL", new Error("socket lane is busy"));
    }

    await this.connected.catch((err) => {
      this.markClosed();
      throw wrapSocketError("TAKO_UNAVAILABLE", err);
    });

    if (this.closed) {
      throw wrapSocketError("TAKO_UNAVAILABLE", new Error("socket is closed"));
    }

    return new Promise<RpcResponse>((resolve, reject) => {
      this.current = { cmd, resolve, reject };
      this.timeout = setTimeout(() => {
        this.failCurrent("TAKO_TIMEOUT", new Error("rpc timed out"));
        this.close();
      }, RPC_TIMEOUT_MS);

      this.socket.write(`${JSON.stringify(cmd)}\n`, (err) => {
        if (!err) return;
        this.failCurrent("TAKO_UNAVAILABLE", err);
        this.close();
      });
    });
  }

  close(): void {
    this.markClosed();
    this.socket.destroy();
  }

  private onData(chunk: Buffer | string): void {
    this.buf += typeof chunk === "string" ? chunk : chunk.toString("utf8");
    const nl = this.buf.indexOf("\n");
    if (nl === -1) return;

    const line = this.buf.slice(0, nl);
    this.buf = this.buf.slice(nl + 1);
    const current = this.takeCurrent();
    if (!current) {
      this.close();
      return;
    }

    try {
      current.resolve(JSON.parse(line) as RpcResponse);
    } catch (err) {
      current.reject(wrapSocketError("TAKO_PROTOCOL", err));
      this.close();
    }
  }

  private failCurrent(code: TakoErrorCode, cause: unknown): void {
    const current = this.takeCurrent();
    if (!current) return;
    current.reject(wrapSocketError(code, cause));
  }

  private takeCurrent(): PendingRpc | null {
    const current = this.current;
    this.current = null;
    if (this.timeout) {
      clearTimeout(this.timeout);
      this.timeout = null;
    }
    return current;
  }

  private markClosed(): void {
    if (this.closed) return;
    this.closed = true;
    this.onClose();
  }
}
