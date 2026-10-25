import { SseReader } from "../channels/sse-reader";
import {
  defaultStoreCache,
  mutateStoreCache,
  registerStoreCacheConsumer,
  storeCacheRevision,
  STORE_CACHE_MAX_AGE_MS,
  STORE_CACHE_MAX_BYTES,
  type StoreCache,
  type StoreCacheEntry,
} from "./cache";

export { clearStoreCache, type StoreCache, type StoreCacheEntry } from "./cache";

export interface StoreOptions {
  params?: Record<string, unknown>;
  baseUrl?: string;
  headers?: Record<string, string>;
  authorization?: string;
  /** Stable non-secret user/tenant identity. Required to persist offline reads. */
  cacheScope?: string;
  /** Bump when the cached response shape changes. Must match the backend version. */
  cacheVersion?: string;
  /** False pauses loading and subscriptions. */
  enabled?: boolean;
  /** Override fetch for nonbrowser runtimes or tests. */
  fetch?: typeof fetch;
  /** Optional offline persistence adapter. Defaults to bounded IndexedDB storage. */
  cache?: StoreCache;
}

export interface StoreSnapshot<T> {
  readonly data: T | undefined;
  readonly status: "loading" | "ready" | "offline" | "error";
  readonly error: Error | null;
  readonly isStale: boolean;
}

export class StoreRequestError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
    this.name = "StoreRequestError";
  }
}

interface OfflinePolicy {
  maxAgeMs: number;
  version: string;
}
interface StoreResponse<T> {
  data: T;
  offline?: OfflinePolicy;
}

const INITIAL_STATE: StoreSnapshot<never> = Object.freeze({
  data: undefined,
  status: "loading",
  error: null,
  isStale: true,
});

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function canonicalJSON(value: unknown): string {
  const ancestors = new Set<object>();
  const normalize = (input: unknown): unknown => {
    if (input === null || typeof input === "string" || typeof input === "boolean") return input;
    if (typeof input === "number" && Number.isFinite(input)) return input;
    if (Array.isArray(input)) {
      if (ancestors.has(input)) throw new Error("Store params must be JSON-serializable.");
      ancestors.add(input);
      const result = input.map(normalize);
      ancestors.delete(input);
      return result;
    }
    if (typeof input === "object" && input !== null) {
      const prototype = Object.getPrototypeOf(input);
      if (prototype !== Object.prototype && prototype !== null) {
        throw new Error("Store params must contain only JSON values.");
      }
      if (ancestors.has(input)) throw new Error("Store params must be JSON-serializable.");
      ancestors.add(input);
      const result: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
      for (const key of Object.keys(input).sort()) {
        const item = (input as Record<string, unknown>)[key];
        if (item !== undefined) result[key] = normalize(item);
      }
      ancestors.delete(input);
      return result;
    }
    throw new Error("Store params must be JSON-serializable.");
  };
  return JSON.stringify(normalize(value));
}

function normalizedParams(params: Record<string, unknown> | undefined): Record<string, unknown> {
  const values: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value !== undefined) values[key] = value;
  }
  return JSON.parse(canonicalJSON(values)) as Record<string, unknown>;
}

function normalizedHeaders(options: StoreOptions): Record<string, string> {
  const headers = new Headers(options.headers);
  if (options.authorization?.trim() && !headers.has("authorization")) {
    const token = options.authorization.trim();
    headers.set("authorization", /^Bearer\s+/i.test(token) ? token : `Bearer ${token}`);
  }
  return Object.fromEntries([...headers.entries()].sort(([a], [b]) => a.localeCompare(b)));
}

function origin(options: StoreOptions): string | undefined {
  const raw = options.baseUrl ?? globalThis.location?.origin;
  if (!raw || raw === "null") return undefined;
  const value = new URL(raw);
  if (value.protocol !== "http:" && value.protocol !== "https:") {
    throw new Error("Store baseUrl must use HTTP or HTTPS.");
  }
  return value.origin;
}

/** @internal Complete runtime identity; auth values stay in memory only. */
export function storeIdentity(name: string, options: StoreOptions): string {
  return JSON.stringify([
    name,
    origin(options),
    normalizedParams(options.params),
    normalizedHeaders(options),
    options.cacheScope,
    options.cacheVersion ?? "1",
    options.enabled !== false,
  ]);
}

function makeURL(base: string | undefined, path: string, params: Record<string, unknown>): URL {
  if (!base) throw new Error("Store operations require a baseUrl outside the browser.");
  const url = new URL(path, base);
  // A typed envelope preserves string/number/null distinctions in union schemas.
  url.searchParams.set("params", JSON.stringify(params));
  return url;
}

function terminalStatus(status: number): boolean {
  return status >= 400 && status < 500 && status !== 408 && status !== 429;
}

/**
 * A live, authorized view of backend data. Subscribe to activate realtime reads;
 * refresh() can also perform a single read without opening a subscription.
 */
export class Store<T = unknown> {
  private readonly options: StoreOptions;
  private readonly base: string | undefined;
  private readonly params: Record<string, unknown>;
  private readonly headers: Record<string, string>;
  private readonly cache: StoreCache;
  private readonly cacheKey: string | undefined;
  private readonly listeners = new Set<() => void>();
  private state: StoreSnapshot<T> = INITIAL_STATE;
  private expiredState: StoreSnapshot<T> = INITIAL_STATE;
  private generation = 0;
  private running = false;
  private blocked = false;
  private dirty = false;
  private streamReady = false;
  private hasData = false;
  private expiresAt: number | undefined;
  private expiryTimer: ReturnType<typeof setTimeout> | undefined;
  private stream: SseReader | undefined;
  private request: AbortController | undefined;
  private pending: Promise<void> | undefined;
  private unregisterCache: (() => void) | undefined;
  private cacheRevision: number;

  constructor(
    readonly name: string,
    options: StoreOptions = {},
  ) {
    if (!name.trim()) throw new Error("Store name must not be empty.");
    if (options.cacheScope !== undefined && !options.cacheScope.trim()) {
      throw new Error("Store cacheScope must not be empty.");
    }
    this.base = origin(options);
    this.params = normalizedParams(options.params);
    this.headers = normalizedHeaders(options);
    this.options = { ...options };
    this.cacheRevision = storeCacheRevision(options.cacheScope);
    this.cache = options.cache ?? defaultStoreCache;
    this.cacheKey = options.cacheScope
      ? JSON.stringify([
          this.base,
          name,
          this.params,
          options.cacheScope,
          options.cacheVersion ?? "1",
        ])
      : undefined;
  }

  /** Immutable snapshot, suitable for useSyncExternalStore. */
  readonly getSnapshot = (): StoreSnapshot<T> => {
    if (this.cacheRevision !== storeCacheRevision(this.options.cacheScope)) return INITIAL_STATE;
    return this.expiresAt !== undefined && Date.now() >= this.expiresAt && this.state.isStale
      ? this.expiredState
      : this.state;
  };
  /** Deterministic SSR snapshot; never accesses browser storage or starts work. */
  readonly getServerSnapshot = (): StoreSnapshot<T> => INITIAL_STATE;

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1 && this.options.enabled !== false) this.start();
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) this.stop();
    };
  };

  /** Reload from the backend. Concurrent invalidations/refreshes are coalesced. */
  readonly refresh = async (): Promise<void> => {
    if (this.options.enabled === false) return;
    this.blocked = false;
    if (!this.running && this.listeners.size > 0) {
      this.start();
      return this.pending;
    }
    this.registerCache();
    const generation = this.generation;
    if (!this.hasData) void this.hydrate(generation);
    try {
      await this.load();
    } finally {
      if (!this.running && generation === this.generation) {
        this.unregisterCache?.();
        this.unregisterCache = undefined;
      }
    }
  };

  /** Stop network activity. Existing subscribers can explicitly refresh to restart. */
  close(): void {
    this.stop();
  }

  private emit(state: StoreSnapshot<T>): void {
    this.state = Object.freeze(state);
    this.expiredState = Object.freeze({
      data: undefined,
      status: state.status === "offline" ? "offline" : "loading",
      error: state.error,
      isStale: true,
    });
    for (const listener of this.listeners) listener();
  }

  private start(): void {
    if (this.running || this.blocked) return;
    this.running = true;
    const generation = ++this.generation;
    this.registerCache();
    if (this.hasData) this.emit({ ...this.state, isStale: true });
    this.expire();
    this.scheduleExpiry();
    void this.hydrate(generation);
    globalThis.addEventListener?.("online", this.onOnline);
    globalThis.addEventListener?.("offline", this.onOffline);
    globalThis.document?.addEventListener("visibilitychange", this.onVisible);
    this.startStream(generation);
    void this.load();
  }

  private registerCache(): void {
    if (this.unregisterCache) return;
    const current = storeCacheRevision(this.options.cacheScope);
    if (this.cacheRevision !== current) {
      this.cacheRevision = current;
      this.state = INITIAL_STATE;
      this.hasData = false;
      this.expiresAt = undefined;
    }
    this.unregisterCache = registerStoreCacheConsumer({
      scope: this.options.cacheScope,
      cache: this.cache,
      clear: () => {
        this.blocked = true;
        this.stop();
        this.hasData = false;
        this.expiresAt = undefined;
        this.cacheRevision = storeCacheRevision(this.options.cacheScope);
        this.emit(INITIAL_STATE);
      },
    });
  }

  private stop(): void {
    this.running = false;
    this.generation++;
    this.stream?.close();
    this.stream = undefined;
    this.streamReady = false;
    this.request?.abort();
    this.request = undefined;
    this.pending = undefined;
    this.dirty = false;
    clearTimeout(this.expiryTimer);
    if (this.hasData) this.emit({ ...this.state, isStale: true });
    this.expire();
    this.unregisterCache?.();
    this.unregisterCache = undefined;
    globalThis.removeEventListener?.("online", this.onOnline);
    globalThis.removeEventListener?.("offline", this.onOffline);
    globalThis.document?.removeEventListener("visibilitychange", this.onVisible);
  }

  private readonly onOnline = (): void => {
    if (!this.blocked) void this.load();
  };
  private readonly onOffline = (): void => {
    this.disconnected(new Error(`Store "${this.name}" is offline.`));
  };
  private readonly onVisible = (): void => {
    this.expire();
    if (globalThis.document?.visibilityState === "visible" && !this.blocked) void this.load();
  };

  private fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
    const fetcher = this.options.fetch ?? globalThis.fetch;
    return fetcher(input, { ...init, credentials: "same-origin" });
  }

  private startStream(generation: number): void {
    try {
      const url = makeURL(
        this.base,
        `/_tako/channels/${encodeURIComponent(`tako.store:${this.name}`)}`,
        this.params,
      );
      const reader = new SseReader(url.toString(), {
        headers: this.headers,
        fetch: (async (input, init) => {
          const response = await this.fetch(input, init);
          if (generation === this.generation && terminalStatus(response.status)) {
            this.fail(
              new StoreRequestError(
                `Store subscription failed (${response.status}).`,
                response.status,
              ),
            );
          }
          return response;
        }) as typeof fetch,
        retryOnDisconnect: true,
        onMessage: ({ data }) => {
          if (generation !== this.generation || this.blocked) return;
          try {
            const message = JSON.parse(data) as { type?: string };
            if (message.type === "tako.store.ready") {
              this.streamReady = true;
              void this.load();
            } else if (message.type === "tako.store.invalidate") {
              void this.load();
            }
          } catch {
            this.disconnected(new Error(`Store "${this.name}" received an invalid update.`));
          }
        },
        onError: (error) => {
          if (generation === this.generation && !this.blocked) this.disconnected(error);
        },
      });
      this.stream = reader;
      void reader.start();
    } catch (error) {
      this.disconnected(asError(error));
    }
  }

  private load(): Promise<void> {
    if (this.blocked) return Promise.resolve();
    this.dirty = true;
    if (this.hasData && !this.state.isStale) this.emit({ ...this.state, isStale: true });
    this.expire();
    if (this.pending) return this.pending;
    const generation = this.generation;
    const pending = (async () => {
      while (this.dirty && !this.blocked && generation === this.generation) {
        this.dirty = false;
        await this.loadOnce(generation);
      }
    })().finally(() => {
      if (this.pending === pending) this.pending = undefined;
    });
    this.pending = pending;
    return pending;
  }

  private async loadOnce(generation: number): Promise<void> {
    const request = new AbortController();
    this.request = request;
    try {
      const url = makeURL(this.base, `/_tako/stores/${encodeURIComponent(this.name)}`, this.params);
      const response = await this.fetch(url, {
        method: "GET",
        headers: this.headers,
        signal: request.signal,
        cache: "no-store",
      });
      if (!response.ok) {
        throw new StoreRequestError(`Store read failed (${response.status}).`, response.status);
      }
      const value = (await response.json()) as StoreResponse<T>;
      if (!value || typeof value !== "object" || !Object.hasOwn(value, "data")) {
        throw new Error("Store response must contain data.");
      }
      if (generation !== this.generation || request.signal.aborted) return;
      this.hasData = true;
      this.expiresAt = undefined;
      const policy = this.policy(value.offline);
      if (policy) this.expiresAt = Date.now() + policy.maxAgeMs;
      this.emit({
        data: value.data,
        status: "ready",
        error: null,
        isStale: !this.streamReady || this.dirty,
      });
      this.scheduleExpiry();
      if (this.cacheKey) {
        if (policy && policy.version === (this.options.cacheVersion ?? "1")) {
          const entry: StoreCacheEntry = {
            key: this.cacheKey,
            scope: this.options.cacheScope!,
            version: policy.version,
            savedAt: Date.now(),
            expiresAt: this.expiresAt!,
            data: value.data,
          };
          try {
            if (
              new TextEncoder().encode(JSON.stringify(entry)).byteLength <= STORE_CACHE_MAX_BYTES
            ) {
              await mutateStoreCache(this.cache, async () => {
                if (generation === this.generation && !request.signal.aborted) {
                  await this.cache.write(entry);
                }
              });
            } else {
              await this.removeCached();
            }
          } catch {
            // Persistence is best effort; the authorized online response is usable.
          }
        } else {
          await this.removeCached().catch(() => {});
        }
      }
    } catch (error) {
      if (generation !== this.generation || request.signal.aborted) return;
      this.fail(asError(error));
    } finally {
      if (this.request === request) this.request = undefined;
    }
  }

  private policy(value: OfflinePolicy | undefined): OfflinePolicy | undefined {
    if (
      !value ||
      typeof value.version !== "string" ||
      !value.version ||
      !Number.isFinite(value.maxAgeMs) ||
      value.maxAgeMs <= 0
    )
      return undefined;
    return { version: value.version, maxAgeMs: Math.min(value.maxAgeMs, STORE_CACHE_MAX_AGE_MS) };
  }

  private async hydrate(generation: number): Promise<void> {
    if (!this.cacheKey) return;
    try {
      const entry = await this.cache.read(this.cacheKey);
      if (generation !== this.generation || this.hasData || !entry) return;
      if (
        entry.key !== this.cacheKey ||
        entry.scope !== this.options.cacheScope ||
        entry.version !== (this.options.cacheVersion ?? "1") ||
        !Number.isFinite(entry.savedAt) ||
        !Number.isFinite(entry.expiresAt) ||
        entry.savedAt > Date.now() ||
        entry.expiresAt <= Date.now() ||
        entry.expiresAt - entry.savedAt > STORE_CACHE_MAX_AGE_MS ||
        !Object.hasOwn(entry, "data") ||
        new TextEncoder().encode(JSON.stringify(entry)).byteLength > STORE_CACHE_MAX_BYTES
      ) {
        await this.removeCached();
        return;
      }
      this.hasData = true;
      this.expiresAt = entry.expiresAt;
      this.emit({
        data: entry.data as T,
        status: this.state.status === "error" ? "offline" : "ready",
        error: this.state.error,
        isStale: true,
      });
      this.scheduleExpiry();
    } catch {
      // Storage may be unavailable, full, or contain an incompatible record.
    }
  }

  private fail(error: Error): void {
    if (error instanceof StoreRequestError && terminalStatus(error.status)) {
      this.blocked = true;
      this.stop();
      this.hasData = false;
      this.expiresAt = undefined;
      this.emit({ data: undefined, status: "error", error, isStale: true });
      if (this.cacheKey) void this.removeCached().catch(() => {});
      return;
    }
    this.disconnected(error, false);
  }

  private disconnected(error: Error, disconnectStream = true): void {
    if (disconnectStream) this.streamReady = false;
    this.emit({
      ...this.state,
      status: this.hasData ? "offline" : "error",
      error,
      isStale: true,
    });
    this.expire();
  }

  private scheduleExpiry(): void {
    clearTimeout(this.expiryTimer);
    if (this.expiresAt === undefined) return;
    this.expiryTimer = setTimeout(() => this.expire(), Math.max(0, this.expiresAt - Date.now()));
    (this.expiryTimer as unknown as { unref?: () => void }).unref?.();
  }

  private expire(): void {
    if (this.expiresAt === undefined || Date.now() < this.expiresAt || !this.state.isStale) return;
    this.hasData = false;
    this.expiresAt = undefined;
    this.emit({
      data: undefined,
      status: this.state.status === "offline" ? "offline" : "loading",
      error: this.state.error,
      isStale: true,
    });
    if (this.cacheKey) void this.removeCached().catch(() => {});
  }

  private removeCached(): Promise<void> {
    const key = this.cacheKey;
    if (!key) return Promise.resolve();
    return mutateStoreCache(this.cache, () => this.cache.remove(key));
  }
}
