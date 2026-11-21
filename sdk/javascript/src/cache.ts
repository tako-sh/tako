import { callInternal, internalSocketFromEnv } from "./tako/socket";

async function request(key: string, args: Record<string, unknown>): Promise<unknown> {
  if (!key.trim()) throw new Error("Cache key cannot be empty.");
  const env = internalSocketFromEnv();
  if (!env) throw new Error("tako.cache requires a Tako runtime with an internal socket.");
  if (Buffer.byteLength(key) > 1024) throw new Error("Cache key exceeds 1 KiB.");
  return callInternal(env.socketPath, { command: "cache", app: env.app, key, ...args });
}

/** Server-owned, per-app JSON cache. Missing and evicted keys both return undefined. */
export const cache = Object.freeze({
  async get<T>(key: string): Promise<T | undefined> {
    const json = await request(key, { operation: "get" });
    return json === null ? undefined : (JSON.parse(json as string) as T);
  },
  async put<T>(key: string, value: T, options: { ttl: number }): Promise<void> {
    if (!Number.isFinite(options.ttl) || options.ttl <= 0) {
      throw new Error("Cache ttl must be a positive number of milliseconds.");
    }
    const json = JSON.stringify(value);
    if (json === undefined) throw new Error("Cache value must be JSON-serializable.");
    if (Buffer.byteLength(json) > 256 * 1024) throw new Error("Cache value exceeds 256 KiB.");
    await request(key, {
      operation: "put",
      json,
      ttl_ms: Math.min(Math.ceil(options.ttl), Number.MAX_SAFE_INTEGER),
    });
  },
  async delete(key: string): Promise<void> {
    await request(key, { operation: "delete" });
  },
});
