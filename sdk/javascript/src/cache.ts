import { callInternal, internalSocketFromEnv } from "./tako/socket";

export interface CacheOptions {
  /** Share this key with other apps on the same server and in the same environment. */
  shared?: boolean;
}

async function request(
  key: string,
  options: CacheOptions | undefined,
  args: Record<string, unknown>,
): Promise<unknown> {
  if (!key.trim()) throw new Error("Cache key cannot be empty.");
  const env = internalSocketFromEnv();
  if (!env) throw new Error("tako.cache requires a Tako runtime with an internal socket.");
  if (Buffer.byteLength(key) > 1024) throw new Error("Cache key exceeds 1 KiB.");
  return callInternal(env.socketPath, {
    command: "cache",
    app: env.app,
    key,
    ...(options?.shared === true && { shared: true }),
    ...args,
  });
}

/** Server-owned JSON cache, private by default. Missing and evicted keys return undefined. */
export const cache = Object.freeze({
  async get<T>(key: string, options?: CacheOptions): Promise<T | undefined> {
    const json = await request(key, options, { operation: "get" });
    return json === null ? undefined : (JSON.parse(json as string) as T);
  },
  async put<T>(key: string, value: T, options: CacheOptions & { ttl: number }): Promise<void> {
    if (!Number.isFinite(options.ttl) || options.ttl <= 0) {
      throw new Error("Cache ttl must be a positive number of milliseconds.");
    }
    const json = JSON.stringify(value);
    if (json === undefined) throw new Error("Cache value must be JSON-serializable.");
    if (Buffer.byteLength(json) > 256 * 1024) throw new Error("Cache value exceeds 256 KiB.");
    await request(key, options, {
      operation: "put",
      json,
      ttl_ms: Math.min(Math.ceil(options.ttl), Number.MAX_SAFE_INTEGER),
    });
  },
  async delete(key: string, options?: CacheOptions): Promise<void> {
    await request(key, options, { operation: "delete" });
  },
});
