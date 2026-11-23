/**
 * Tako-hosted KV stores, exposed as `tako.kv`.
 *
 * Each binding in `[envs.<env>].kv` is a store of JSON values, with an
 * optional TTL per key. Keys stay until deleted unless a TTL is set:
 *
 * ```ts
 * import { tako } from "tako.sh";
 *
 * await tako.kv.sessions.put("session:42", { user: "u1" }, { ttl: 86_400_000 });
 * const session = await tako.kv.sessions.get<{ user: string }>("session:42");
 * await tako.kv.sessions.delete("session:42");
 * ```
 *
 * Requests go through the internal socket with the binding's token; the
 * store may live on another server of the network.
 */

import { getKvBindings } from "./tako/secrets";
import { callInternal, internalSocketFromEnv } from "./tako/socket";

/** The longest key, in UTF-8 bytes. */
const MAX_KEY_BYTES = 1024;
/** The largest value, as JSON text in UTF-8 bytes. */
const MAX_VALUE_BYTES = 256 * 1024;

/** Options for {@link TakoKvStore.put}. */
export interface KvPutOptions {
  /**
   * Milliseconds until the key expires. Without it, the key stays until it
   * is deleted. Putting the key again replaces its TTL.
   */
  ttl?: number;
}

/** A Tako-hosted KV store of JSON values. */
export interface TakoKvStore {
  /** The key's value, or undefined when it is not set or has expired. */
  get<T = unknown>(key: string): Promise<T | undefined>;
  /** Sets the key to `value`, which must be JSON-serializable. */
  put<T>(key: string, value: T, options?: KvPutOptions): Promise<void>;
  /** Removes the key. Removing a key that is not set succeeds. */
  delete(key: string): Promise<void>;
}

/** Typed KV store bag exposed as `tako.kv`. */
export type TakoKvBag<T = Record<never, never>> = Readonly<{
  [K in keyof T]: TakoKvStore;
}> &
  Readonly<Record<string, TakoKvStore | undefined>>;

/** Load KV store bindings delivered by the Tako runtime. */
export function loadKv<T = Record<never, never>>(): TakoKvBag<T> {
  return createKvBag<T>(getKvBindings);
}

/**
 * Create a typed KV store bag over raw binding payloads. Bindings are read on
 * access, so the bag reflects the bootstrap even when imported before it.
 *
 * @internal
 */
export function createKvBag<T = Record<never, never>>(
  bindings: () => Record<string, unknown>,
): TakoKvBag<T> {
  const cache = new WeakMap<object, TakoKvStore>();
  const lookup = (name: string): TakoKvStore | undefined => {
    const all = bindings();
    if (!Object.prototype.hasOwnProperty.call(all, name)) return undefined;
    const raw = all[name];
    if (typeof raw !== "object" || raw === null) {
      throw new Error(`Tako: KV binding "${name}" is malformed`);
    }
    const cached = cache.get(raw);
    if (cached) return cached;
    const store = createKvStore(name, raw as Record<string, unknown>);
    cache.set(raw, store);
    return store;
  };

  return new Proxy(Object.create(null) as Record<string, TakoKvStore>, {
    get(_target, prop: string | symbol): unknown {
      if (typeof prop !== "string") return undefined;
      return lookup(prop);
    },
    ownKeys(): string[] {
      return Object.keys(bindings());
    },
    getOwnPropertyDescriptor(_target, prop: string | symbol) {
      if (typeof prop !== "string") return undefined;
      const store = lookup(prop);
      // Enumerable, so Object.keys, entries, and spreading see the bindings.
      // A store keeps its token out of reach of logging and serializing.
      return store ? { configurable: true, enumerable: true, value: store } : undefined;
    },
    has(_target, prop: string | symbol): boolean {
      return typeof prop === "string" && Object.prototype.hasOwnProperty.call(bindings(), prop);
    },
  }) as TakoKvBag<T>;
}

function createKvStore(name: string, raw: Record<string, unknown>): TakoKvStore {
  const { token } = raw;
  if (typeof token !== "string" || token === "") {
    throw new Error(`Tako: KV binding "${name}" must have a token`);
  }
  const request = (key: string, args: Record<string, unknown>): Promise<unknown> => {
    if (typeof key !== "string" || key === "") throw new Error("KV key cannot be empty.");
    if (Buffer.byteLength(key) > MAX_KEY_BYTES) throw new Error("KV key exceeds 1 KiB.");
    const env = internalSocketFromEnv();
    if (!env) throw new Error("tako.kv requires a Tako runtime with an internal socket.");
    return callInternal(env.socketPath, { command: "kv", app: env.app, token, key, ...args });
  };
  const store: TakoKvStore = {
    async get<T>(key: string): Promise<T | undefined> {
      const json = await request(key, { operation: "get" });
      return json === null ? undefined : (JSON.parse(json as string) as T);
    },
    async put<T>(key: string, value: T, options?: KvPutOptions): Promise<void> {
      const ttl = options?.ttl;
      if (ttl !== undefined && (!Number.isFinite(ttl) || ttl <= 0)) {
        throw new Error("KV ttl must be a positive number of milliseconds.");
      }
      const json = JSON.stringify(value);
      if (json === undefined) throw new Error("KV value must be JSON-serializable.");
      if (Buffer.byteLength(json) > MAX_VALUE_BYTES) throw new Error("KV value exceeds 256 KiB.");
      await request(key, {
        operation: "put",
        json,
        ...(ttl !== undefined && { ttl_ms: Math.min(Math.ceil(ttl), Number.MAX_SAFE_INTEGER) }),
      });
    },
    async delete(key: string): Promise<void> {
      await request(key, { operation: "delete" });
    },
  };
  const described = () => `[TakoKvStore ${name}]`;
  Object.defineProperties(store, {
    toJSON: { value: () => ({ kv: name }), enumerable: false },
    [Symbol.for("nodejs.util.inspect.custom")]: { value: described, enumerable: false },
  });
  return Object.freeze(store);
}
