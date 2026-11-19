/**
 * Secrets + storage + internal-auth-token proxy store. Pure, isomorphic-safe — the
 * fd-pipe reader that actually populates this state lives in
 * `./secrets-fd.ts` so that `tako.sh/internal` can re-export
 * `loadSecrets` without dragging `node:fs` into consumer graphs.
 *
 * Tako spawns each app process with a pipe on fd 3 containing a JSON
 * envelope `{"token": ..., "secrets": {...}, "storages": {...}, "databases": {...}}`.
 * Server/worker entrypoints read the envelope and call `injectBootstrap(...)`
 * before the user's module is imported. Processes without an entrypoint, such
 * as scripts under `tako run`, receive it in `TAKO_BOOTSTRAP_DATA`, which is
 * read on first access.
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

/** Bootstrap payload injected into app and worker processes by Tako. */
export interface BootstrapEnvelope {
  /** Internal request token; null outside a managed Tako process. */
  token: string | null;
  /** Secret values keyed by configured secret name. */
  secrets: Record<string, string>;
  /** Storage binding metadata keyed by binding name. */
  storages?: Record<string, unknown> | undefined;
  /** Database connections keyed by binding name. */
  databases?: Record<string, unknown> | undefined;
}

interface BootstrapState {
  token: string | null;
  secrets: Record<string, string>;
  storages: Record<string, unknown>;
  databases: Record<string, unknown>;
}

const BOOTSTRAP_ENV = "TAKO_BOOTSTRAP_DATA";
/** Set by Tako on every Compose service with `x-tako`. */
const COMPOSE_SERVICE_ENV = "TAKO_COMPOSE_SERVICE";

let bootstrap: BootstrapState = { token: null, secrets: {}, storages: {}, databases: {} };
let injected = false;

/** Low-level: replace the whole bootstrap state (tests + fd-reader init). */
export function injectBootstrap(next: BootstrapEnvelope): void {
  injected = true;
  bootstrap = {
    token: next.token,
    secrets: Object.assign(Object.create(null), next.secrets ?? {}),
    storages: Object.assign(Object.create(null), next.storages ?? {}),
    databases: Object.assign(Object.create(null), next.databases ?? {}),
  };
}

/**
 * Parse a bootstrap envelope. Throws an `Error` that describes the expected
 * shape when `data` is not a valid envelope.
 */
export function parseBootstrapEnvelope(data: string): BootstrapEnvelope {
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    throw new Error("Tako: invalid bootstrap JSON");
  }
  if (
    !isObject(parsed) ||
    typeof parsed["token"] !== "string" ||
    !isObject(parsed["secrets"]) ||
    !optionalObject(parsed, "storages") ||
    !optionalObject(parsed, "databases")
  ) {
    throw new Error(
      "Tako: bootstrap must be {token: string, secrets: object, storages?: object, databases?: object}",
    );
  }
  return {
    token: parsed["token"],
    secrets: parsed["secrets"] as Record<string, string>,
    storages: parsed["storages"] as Record<string, unknown> | undefined,
    databases: parsed["databases"] as Record<string, unknown> | undefined,
  };
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function optionalObject(value: Record<string, unknown>, key: string): boolean {
  return !(key in value) || isObject(value[key]);
}

/**
 * The bootstrap state. Without an injected envelope, consumes
 * `TAKO_BOOTSTRAP_DATA` once, which `tako run` sets for plain scripts.
 */
function current(): BootstrapState {
  if (!injected) {
    injected = true;
    const env = typeof process !== "undefined" ? process.env : undefined;
    const data = env?.[BOOTSTRAP_ENV];
    if (typeof data === "string" && data.length > 0) {
      delete env![BOOTSTRAP_ENV];
      injectBootstrap(parseBootstrapEnvelope(data));
    }
  }
  return bootstrap;
}

/**
 * Tako gives every Compose `x-tako` service `TAKO_COMPOSE_SERVICE`, but the
 * runtime (bootstrap, socket) only to `sdk: true` services. Returns why a
 * runtime feature is unavailable in such a service, or `null` otherwise.
 * Never throws, so plain services can still import the SDK for utilities.
 */
export function composeSdkHint(): string | null {
  const env = typeof process !== "undefined" ? process.env : undefined;
  const service = env?.[COMPOSE_SERVICE_ENV];
  if (!service || current().token !== null) return null;
  return (
    `Tako: this Compose service ('${service}') isn't marked \`sdk: true\` in x-tako, ` +
    `so it has no Tako runtime for secrets, channels or workflows. ` +
    `Add \`sdk: true\` to services.${service}.x-tako in the Compose file.`
  );
}

/** Returns the internal auth token, or `null` when running outside Tako. */
export function getInternalToken(): string | null {
  return current().token;
}

/** Returns storage binding payloads injected by Tako at process startup. */
export function getStorageBindings(): Record<string, unknown> {
  return current().storages;
}

/** Returns database connection payloads injected by Tako at process startup. */
export function getDatabaseBindings(): Record<string, unknown> {
  return current().databases;
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
      if (typeof prop === "string") {
        const hint = prop === "then" ? null : composeSdkHint();
        if (hint) throw new Error(hint);
        return current().secrets[prop];
      }
      return undefined;
    },
    ownKeys(): string[] {
      return Object.keys(current().secrets);
    },
    getOwnPropertyDescriptor(_target, prop: string | symbol) {
      const secrets = current().secrets;
      if (typeof prop === "string" && prop in secrets) {
        return { configurable: true, enumerable: false, value: secrets[prop] };
      }
      return undefined;
    },
    has(_target, prop: string | symbol): boolean {
      return typeof prop === "string" && prop in current().secrets;
    },
  }) as Readonly<T>;
}
