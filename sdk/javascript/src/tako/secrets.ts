/**
 * Secrets + storage + internal-auth-token proxy store. Pure, isomorphic-safe — the
 * fd-pipe reader that actually populates this state lives in
 * `./secrets-fd.ts` so that `tako.sh/internal` can re-export
 * `loadSecrets` without dragging `node:fs` into consumer graphs.
 *
 * Tako spawns each app process with a pipe on fd 3 containing a JSON
 * envelope `{"token": ..., "secrets": {...}, "storages": {...}}`.
 * Server/worker entrypoints read the envelope and call `injectBootstrap(...)`
 * before the user's module is imported.
 *
 * The token is kept in module scope and used by the SDK to authenticate
 * server-issued `Host: <app>.tako` requests — it is not exposed to
 * user code, and it does NOT leak to processes the app spawns (unlike
 * an env var would).
 *
 * Secrets are exposed through the `tako.secrets` proxy exported from
 * `tako.sh`. Its `toString`/`toJSON`/inspect return
 * `[REDACTED]` and its property descriptors are non-enumerable, so
 * bulk-spread (`{ ...secrets }`) returns an empty object — individual
 * access via `secrets.KEY` still works through the `get` trap.
 */

export interface BootstrapEnvelope {
  token: string | null;
  secrets: Record<string, string>;
  storages?: Record<string, unknown> | undefined;
}

interface BootstrapState {
  token: string | null;
  secrets: Record<string, string>;
  storages: Record<string, unknown>;
}

let bootstrap: BootstrapState = { token: null, secrets: {}, storages: {} };

/** Low-level: replace the whole bootstrap state (tests + fd-reader init). */
export function injectBootstrap(next: BootstrapEnvelope): void {
  bootstrap = {
    token: next.token,
    secrets: Object.assign(Object.create(null), next.secrets ?? {}),
    storages: Object.assign(Object.create(null), next.storages ?? {}),
  };
}

/** Returns the internal auth token, or `null` when running outside Tako. */
export function getInternalToken(): string | null {
  return bootstrap.token;
}

/** Returns storage binding payloads injected by Tako at process startup. */
export function getStorageBindings(): Record<string, unknown> {
  return bootstrap.storages;
}

/**
 * Build the proxy-backed accessor that becomes `tako.secrets`. The generated
 * `tako.d.ts` file augments `TakoSecrets` so individual key access
 * (`tako.secrets.FOO`) is typed as a readonly field — `tako.secrets.FOO = "x"`
 * is a compile error.
 */
export function loadSecrets<T = Record<string, string>>(): Readonly<T> {
  return new Proxy(Object.create(null) as Record<string, string>, {
    get(_target, prop: string | symbol): unknown {
      if (prop === "toString" || prop === "toJSON") return () => "[REDACTED]";
      if (prop === Symbol.for("nodejs.util.inspect.custom")) return () => "[REDACTED]";
      if (prop === Symbol.toPrimitive) return () => "[REDACTED]";
      if (typeof prop === "string") return bootstrap.secrets[prop];
      return undefined;
    },
    ownKeys(): string[] {
      return Object.keys(bootstrap.secrets);
    },
    getOwnPropertyDescriptor(_target, prop: string | symbol) {
      if (typeof prop === "string" && prop in bootstrap.secrets) {
        return { configurable: true, enumerable: false, value: bootstrap.secrets[prop] };
      }
      return undefined;
    },
    has(_target, prop: string | symbol): boolean {
      return typeof prop === "string" && prop in bootstrap.secrets;
    },
  }) as Readonly<T>;
}
