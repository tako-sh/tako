/**
 * Tako-hosted KV stores, exposed as `tako.kv`.
 *
 * Each binding is a plain `{ url }` connection for the Redis protocol. The
 * URL carries the binding's token as its password, so any Redis client
 * connects with it as is:
 *
 * ```ts
 * import { RedisClient } from "bun";
 * import { tako } from "tako.sh";
 *
 * const kv = new RedisClient(tako.kv.sessions.url);
 * ```
 */

import { getKvBindings } from "./tako/secrets";

/** Connection to a Tako-hosted KV store. */
export interface TakoKvStore {
  /**
   * `redis://` URL of the store, served by Tako next to the app. Its password
   * is the token that authorizes this app's binding. Treat it as a secret.
   */
  readonly url: string;
}

/** Typed KV store bag exposed as `tako.kv`. */
export type TakoKvBag<T = Record<never, never>> = Readonly<{
  [K in keyof T]: TakoKvStore;
}> &
  Readonly<Record<string, TakoKvStore | undefined>>;

const REDACTED = "[REDACTED]";

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
      // Enumerable, so Object.keys, entries, and spreading see the bindings;
      // each connection redacts its token when logged or serialized.
      return store ? { configurable: true, enumerable: true, value: store } : undefined;
    },
    has(_target, prop: string | symbol): boolean {
      return typeof prop === "string" && Object.prototype.hasOwnProperty.call(bindings(), prop);
    },
  }) as TakoKvBag<T>;
}

function createKvStore(name: string, raw: Record<string, unknown>): TakoKvStore {
  const { url } = raw;
  if (typeof url !== "string") {
    throw new Error(`Tako: KV binding "${name}" must have a url`);
  }
  const store = { url };
  const redacted = () => ({ url: redactKvUrl(url) });
  // Logging or serializing a connection must not print its token.
  Object.defineProperties(store, {
    toJSON: { value: redacted, enumerable: false },
    [Symbol.for("nodejs.util.inspect.custom")]: { value: redacted, enumerable: false },
  });
  return Object.freeze(store);
}

/**
 * `redis://:<token>@host:port` as `redis://:[REDACTED]@host:port`. Splits at
 * the last `@` so a password containing one never leaks a part of it.
 */
function redactKvUrl(url: string): string {
  const at = url.lastIndexOf("@");
  if (at === -1) return REDACTED;
  const schemeEnd = url.indexOf("://");
  const scheme = schemeEnd !== -1 && schemeEnd < at ? url.slice(0, schemeEnd + 3) : "";
  return `${scheme}:${REDACTED}@${url.slice(at + 1)}`;
}
