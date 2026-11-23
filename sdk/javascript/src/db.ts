/**
 * Tako-hosted databases, exposed as `tako.db`.
 *
 * Each binding is a plain `{ url, authToken }` connection for the libSQL
 * HTTP protocol, so it can be passed straight to an existing driver:
 *
 * ```ts
 * import { createClient } from "@libsql/client";
 * import { tako } from "tako.sh";
 *
 * const db = createClient(tako.db.main);
 * ```
 */

import { getDatabaseBindings } from "./tako/secrets";

/** Connection to a Tako-hosted database. */
export interface TakoDbConnection {
  /** Endpoint of the database, served by Tako next to the app. */
  readonly url: string;
  /** Token that authorizes this app's binding. Treat it as a secret. */
  readonly authToken: string;
}

/** Typed database bag exposed as `tako.db`. */
export type TakoDbBag<T = Record<never, never>> = Readonly<{
  [K in keyof T]: TakoDbConnection;
}> &
  Readonly<Record<string, TakoDbConnection | undefined>>;

const REDACTED = "[REDACTED]";

/** Load database bindings delivered by the Tako runtime. */
export function loadDb<T = Record<never, never>>(): TakoDbBag<T> {
  return createDbBag<T>(getDatabaseBindings);
}

/**
 * Create a typed database bag over raw binding payloads. Bindings are read on
 * access, so the bag reflects the bootstrap even when imported before it.
 *
 * @internal
 */
export function createDbBag<T = Record<never, never>>(
  bindings: () => Record<string, unknown>,
): TakoDbBag<T> {
  const cache = new WeakMap<object, TakoDbConnection>();
  const lookup = (name: string): TakoDbConnection | undefined => {
    const all = bindings();
    if (!Object.prototype.hasOwnProperty.call(all, name)) return undefined;
    const raw = all[name];
    if (typeof raw !== "object" || raw === null) {
      throw new Error(`Tako: database binding "${name}" is malformed`);
    }
    const cached = cache.get(raw);
    if (cached) return cached;
    const database = createDatabase(name, raw as Record<string, unknown>);
    cache.set(raw, database);
    return database;
  };

  return new Proxy(Object.create(null) as Record<string, TakoDbConnection>, {
    get(_target, prop: string | symbol): unknown {
      if (typeof prop !== "string") return undefined;
      return lookup(prop);
    },
    ownKeys(): string[] {
      return Object.keys(bindings());
    },
    getOwnPropertyDescriptor(_target, prop: string | symbol) {
      if (typeof prop !== "string") return undefined;
      const database = lookup(prop);
      // Enumerable, so Object.keys, entries, and spreading see the bindings;
      // each connection redacts its token when logged or serialized.
      return database ? { configurable: true, enumerable: true, value: database } : undefined;
    },
    has(_target, prop: string | symbol): boolean {
      return typeof prop === "string" && Object.prototype.hasOwnProperty.call(bindings(), prop);
    },
  }) as TakoDbBag<T>;
}

function createDatabase(name: string, raw: Record<string, unknown>): TakoDbConnection {
  const { url, authToken } = raw;
  if (typeof url !== "string" || typeof authToken !== "string") {
    throw new Error(`Tako: database binding "${name}" must have a url and an authToken`);
  }
  const database = { url, authToken };
  const redacted = () => ({ url, authToken: REDACTED });
  // Logging or serializing a connection must not print its token.
  Object.defineProperties(database, {
    toJSON: { value: redacted, enumerable: false },
    [Symbol.for("nodejs.util.inspect.custom")]: { value: redacted, enumerable: false },
  });
  return Object.freeze(database);
}
