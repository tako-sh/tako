import { afterEach, describe, expect, test } from "bun:test";
import { indexedDB } from "fake-indexeddb";
import { defaultStoreCache } from "../src/stores/cache";
import {
  Store,
  clearStoreCache,
  storeIdentity,
  type StoreCache,
  type StoreCacheEntry,
} from "../src/stores/client";

interface PendingRead {
  url: URL;
  init: RequestInit | undefined;
  resolve(value: unknown, offline?: { maxAgeMs: number; version: string }): void;
  response(status: number): void;
  reject(error: Error): void;
}

class Backend {
  readonly reads: PendingRead[] = [];
  readonly streams: Array<{
    send(type: string): void;
    close(): void;
    headers: Headers;
  }> = [];
  streamStatus = 200;
  readonly fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    if (url.pathname.startsWith("/_tako/channels/")) {
      if (this.streamStatus !== 200) return new Response(null, { status: this.streamStatus });
      let closed = false;
      let controller!: ReadableStreamDefaultController<Uint8Array>;
      const stream = new ReadableStream<Uint8Array>({
        start(value) {
          controller = value;
        },
        cancel() {
          closed = true;
        },
      });
      const close = () => {
        if (!closed) {
          closed = true;
          controller.close();
        }
      };
      init?.signal?.addEventListener("abort", close, { once: true });
      this.streams.push({
        send(type) {
          if (!closed)
            controller.enqueue(
              new TextEncoder().encode(`data: ${JSON.stringify({ type, data: {} })}\n\n`),
            );
        },
        close,
        headers: new Headers(init?.headers),
      });
      return new Response(stream, { headers: { "Content-Type": "text/event-stream" } });
    }
    return new Promise<Response>((resolve, reject) => {
      this.reads.push({
        url,
        init,
        resolve: (data, offline) => resolve(Response.json({ data, ...(offline && { offline }) })),
        response: (status) => resolve(new Response(null, { status })),
        reject,
      });
    });
  }) as typeof fetch;
}

class MemoryCache implements StoreCache {
  readonly entries = new Map<string, StoreCacheEntry>();
  async read(key: string): Promise<StoreCacheEntry | undefined> {
    const value = this.entries.get(key);
    return value ? structuredClone(value) : undefined;
  }
  async write(entry: StoreCacheEntry): Promise<void> {
    this.entries.set(entry.key, structuredClone(entry));
  }
  async remove(key: string): Promise<void> {
    this.entries.delete(key);
  }
  async clear(scope?: string): Promise<void> {
    for (const [key, entry] of this.entries) {
      if (scope === undefined || entry.scope === scope) this.entries.delete(key);
    }
  }
}

const cleanups: Array<() => void> = [];
function watch<T>(store: Store<T>): void {
  cleanups.push(store.subscribe(() => {}));
}
async function until(condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (condition()) return;
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  throw new Error("Condition did not become true.");
}
afterEach(() => {
  for (const cleanup of cleanups.splice(0)) cleanup();
});

describe("Store realtime reads", () => {
  test("loads promptly, catches up on ready, and coalesces invalidations during a load", async () => {
    const backend = new Backend();
    const store = new Store<number>("messages", {
      baseUrl: "https://app.test",
      fetch: backend.fetch,
    });
    watch(store);
    expect(backend.reads).toHaveLength(1);
    backend.reads[0]!.resolve(1);
    await until(() => store.getSnapshot().data === 1);
    expect(store.getSnapshot().isStale).toBe(true);
    backend.streams[0]!.send("tako.store.ready");
    await until(() => backend.reads.length === 2);
    backend.streams[0]!.send("tako.store.invalidate");
    backend.streams[0]!.send("tako.store.invalidate");
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(backend.reads).toHaveLength(2);
    backend.reads[1]!.resolve(2);
    await until(() => backend.reads.length === 3);
    backend.reads[2]!.resolve(3);
    await until(() => store.getSnapshot().data === 3);
    expect(store.getSnapshot()).toMatchObject({ status: "ready", error: null, isStale: false });
  });

  test("refetches on a reconnect ready even when no invalidation was received", async () => {
    const backend = new Backend();
    const store = new Store("messages", { baseUrl: "https://app.test", fetch: backend.fetch });
    watch(store);
    backend.reads[0]!.resolve("before");
    await until(() => store.getSnapshot().data === "before");
    backend.streams[0]!.send("tako.store.ready");
    await until(() => backend.reads.length === 2);
    backend.reads[1]!.resolve("caught up");
    await until(() => store.getSnapshot().data === "caught up");
    backend.streams[0]!.close();
    await new Promise((resolve) => setTimeout(resolve, 1400));
    await until(() => backend.streams.length === 2);
    backend.streams[1]!.send("tako.store.ready");
    await until(() => backend.reads.length === 3);
    backend.reads[2]!.resolve("after disconnect");
    await until(() => store.getSnapshot().data === "after disconnect");
  });

  test("normalizes params and sends identical credentials on HTTP and SSE", async () => {
    const backend = new Backend();
    const store = new Store("message list", {
      baseUrl: "https://app.test/ignored",
      params: { room: "a/b", object: { z: 2, a: 1 }, ids: [1, 2], empty: null },
      headers: { "X-Trace": "test" },
      authorization: "secret",
      fetch: backend.fetch,
    });
    watch(store);
    const read = backend.reads[0]!;
    expect(read.url.pathname).toBe("/_tako/stores/message%20list");
    expect(JSON.parse(read.url.searchParams.get("params")!)).toEqual({
      room: "a/b",
      object: { a: 1, z: 2 },
      ids: [1, 2],
      empty: null,
    });
    expect(new Headers(read.init?.headers).get("authorization")).toBe("Bearer secret");
    expect(backend.streams[0]!.headers.get("authorization")).toBe("Bearer secret");
    expect(read.init?.credentials).toBe("same-origin");
    expect(read.init?.cache).toBe("no-store");
  });

  test("late responses cannot restore a stopped subscription", async () => {
    const backend = new Backend();
    const store = new Store("messages", { baseUrl: "https://app.test", fetch: backend.fetch });
    const unsubscribe = store.subscribe(() => {});
    unsubscribe();
    expect(backend.reads[0]!.init?.signal?.aborted).toBe(true);
    backend.reads[0]!.resolve("old data");
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(store.getSnapshot().data).toBeUndefined();
    watch(store);
    backend.reads[1]!.resolve("new data");
    await until(() => store.getSnapshot().data === "new data");
  });

  test("disabled stores and SSR snapshots do not start work", () => {
    const backend = new Backend();
    const store = new Store("messages", { enabled: false, fetch: backend.fetch });
    expect(store.getServerSnapshot()).toBe(store.getServerSnapshot());
    watch(store);
    expect(backend.reads).toHaveLength(0);
    expect(backend.streams).toHaveLength(0);
  });

  test("a transient read failure does not lose a healthy subscription's freshness", async () => {
    const backend = new Backend();
    const store = new Store("messages", { baseUrl: "https://app.test", fetch: backend.fetch });
    watch(store);
    backend.reads[0]!.resolve("first");
    await until(() => store.getSnapshot().data === "first");
    backend.streams[0]!.send("tako.store.ready");
    await until(() => backend.reads.length === 2);
    backend.reads[1]!.resolve("subscribed");
    await until(() => store.getSnapshot().isStale === false);
    const failed = store.refresh();
    expect(store.getSnapshot().isStale).toBe(true);
    backend.reads[2]!.response(500);
    await failed;
    expect(store.getSnapshot().status).toBe("offline");
    const retry = store.refresh();
    backend.reads[3]!.resolve("recovered");
    await retry;
    expect(store.getSnapshot()).toMatchObject({
      data: "recovered",
      status: "ready",
      isStale: false,
    });
  });

  test("special JSON property names are preserved without changing object prototypes", () => {
    const backend = new Backend();
    const params = JSON.parse('{"__proto__":{"private":true},"type":"1"}') as Record<
      string,
      unknown
    >;
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      fetch: backend.fetch,
      params,
    });
    watch(store);
    expect(backend.reads[0]!.url.searchParams.get("params")).toBe(
      '{"__proto__":{"private":true},"type":"1"}',
    );
    expect(() => new Store("messages", { params: { date: new Date() } })).toThrow("JSON values");
  });
});

describe("Store offline reads", () => {
  test("requires both explicit cache scope and backend policy", async () => {
    const backend = new Backend();
    const cache = new MemoryCache();
    const noScope = new Store("public", {
      baseUrl: "https://app.test",
      fetch: backend.fetch,
      cache,
    });
    watch(noScope);
    backend.reads[0]!.resolve("value", { maxAgeMs: 10_000, version: "1" });
    await until(() => noScope.getSnapshot().data === "value");
    expect(cache.entries.size).toBe(0);
    const noPolicy = new Store("private", {
      baseUrl: "https://app.test",
      fetch: backend.fetch,
      cache,
      cacheScope: "alice",
    });
    watch(noPolicy);
    backend.reads[1]!.resolve("private value");
    await until(() => noPolicy.getSnapshot().data === "private value");
    expect(cache.entries.size).toBe(0);
  });

  test("persists without credentials and restores only the same origin, scope, params, version", async () => {
    const cache = new MemoryCache();
    const firstBackend = new Backend();
    const options = {
      baseUrl: "https://app.test",
      params: { room: "one" },
      authorization: "SECRET_TOKEN",
      cacheScope: "alice",
      cacheVersion: "2",
      cache,
    };
    const first = new Store("messages", { ...options, fetch: firstBackend.fetch });
    watch(first);
    firstBackend.reads[0]!.resolve(["cached"], { maxAgeMs: 10_000, version: "2" });
    await until(() => cache.entries.size === 1);
    expect(JSON.stringify([...cache.entries])).not.toContain("SECRET_TOKEN");
    first.close();
    const offlineBackend = new Backend();
    const cached = new Store("messages", {
      ...options,
      authorization: "rotated token",
      fetch: offlineBackend.fetch,
    });
    watch(cached);
    offlineBackend.reads[0]!.reject(new Error("offline"));
    await until(() => cached.getSnapshot().status === "offline");
    expect(cached.getSnapshot().data).toEqual(["cached"]);
    const other = new Store("messages", {
      ...options,
      cacheScope: "bob",
      fetch: offlineBackend.fetch,
    });
    watch(other);
    offlineBackend.reads[1]!.reject(new Error("offline"));
    await until(() => other.getSnapshot().status === "error");
    expect(other.getSnapshot().data).toBeUndefined();
    const changedVersion = new Store("messages", {
      ...options,
      cacheVersion: "3",
      fetch: offlineBackend.fetch,
    });
    watch(changedVersion);
    offlineBackend.reads[2]!.reject(new Error("offline"));
    await until(() => changedVersion.getSnapshot().status === "error");
    expect(changedVersion.getSnapshot().data).toBeUndefined();
  });

  test("expires hydrated data in memory while offline", async () => {
    const cache = new MemoryCache();
    const backend = new Backend();
    const options = {
      baseUrl: "https://app.test",
      cacheScope: "alice",
      cache,
      fetch: backend.fetch,
    };
    const first = new Store("messages", options);
    watch(first);
    backend.reads[0]!.resolve("short lived", { maxAgeMs: 100, version: "1" });
    await until(() => cache.entries.size === 1);
    first.close();
    const offline = new Store("messages", options);
    watch(offline);
    backend.reads[1]!.reject(new Error("offline"));
    await until(() => offline.getSnapshot().data === "short lived");
    await new Promise((resolve) => setTimeout(resolve, 120));
    expect(offline.getSnapshot().data).toBeUndefined();
    expect(offline.getSnapshot().status).toBe("offline");
    await until(() => cache.entries.size === 0);
  });

  test("authorization failure purges cached data and stops the stream", async () => {
    const cache = new MemoryCache();
    const backend = new Backend();
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      cacheScope: "alice",
      cache,
      fetch: backend.fetch,
    });
    watch(store);
    backend.reads[0]!.resolve("private", { maxAgeMs: 10_000, version: "1" });
    await until(() => cache.entries.size === 1);
    const refreshing = store.refresh();
    await until(() => backend.reads.length === 2);
    backend.reads[1]!.response(403);
    await refreshing;
    await until(() => cache.entries.size === 0);
    expect(store.getSnapshot()).toMatchObject({ data: undefined, status: "error" });
    backend.streams[0]!.send("tako.store.invalidate");
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(backend.reads).toHaveLength(2);
  });

  test("a forbidden subscription aborts an in-flight HTTP read", async () => {
    const backend = new Backend();
    backend.streamStatus = 403;
    const store = new Store("private", { baseUrl: "https://app.test", fetch: backend.fetch });
    watch(store);
    await until(() => store.getSnapshot().status === "error");
    expect(backend.reads[0]!.init?.signal?.aborted).toBe(true);
    backend.reads[0]!.resolve("must not display");
    await new Promise((resolve) => setTimeout(resolve, 5));
    expect(store.getSnapshot().data).toBeUndefined();
  });

  test("logout waits for an old cache write and prevents repopulation", async () => {
    const cache = new MemoryCache();
    let finishWrite!: () => void;
    let writing = false;
    const write = cache.write.bind(cache);
    cache.write = async (entry) => {
      writing = true;
      await new Promise<void>((resolve) => {
        finishWrite = resolve;
      });
      await write(entry);
    };
    const backend = new Backend();
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      cacheScope: "alice",
      cache,
      fetch: backend.fetch,
    });
    watch(store);
    backend.reads[0]!.resolve("private", { maxAgeMs: 10_000, version: "1" });
    await until(() => writing);
    const clearing = clearStoreCache("alice");
    expect(store.getSnapshot().data).toBeUndefined();
    finishWrite();
    await clearing;
    expect(cache.entries.size).toBe(0);
    expect(backend.reads[0]!.init?.signal?.aborted).toBe(true);
  });

  test("logout also fences a standalone refresh without any subscribers", async () => {
    const cache = new MemoryCache();
    const backend = new Backend();
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      cacheScope: "standalone",
      cache,
      fetch: backend.fetch,
    });
    const reading = store.refresh();
    await clearStoreCache("standalone");
    expect(backend.reads[0]!.init?.signal?.aborted).toBe(true);
    backend.reads[0]!.resolve("too late", { maxAgeMs: 10_000, version: "1" });
    await reading;
    expect(cache.entries.size).toBe(0);
    expect(store.getSnapshot().data).toBeUndefined();
  });

  test("logout clears a completed standalone read's adapter and retained snapshot", async () => {
    const cache = new MemoryCache();
    const backend = new Backend();
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      cacheScope: "completed",
      cache,
      fetch: backend.fetch,
    });
    const reading = store.refresh();
    backend.reads[0]!.resolve("private", { maxAgeMs: 10_000, version: "1" });
    await reading;
    expect(cache.entries.size).toBe(1);
    await clearStoreCache("completed");
    expect(cache.entries.size).toBe(0);
    expect(store.getSnapshot().data).toBeUndefined();
  });

  test("resubscribing cannot briefly expose data whose offline lease expired while stopped", async () => {
    const backend = new Backend();
    const store = new Store("messages", { baseUrl: "https://app.test", fetch: backend.fetch });
    const unsubscribe = store.subscribe(() => {});
    backend.reads[0]!.resolve("temporary", { maxAgeMs: 20, version: "1" });
    await until(() => store.getSnapshot().data === "temporary");
    unsubscribe();
    expect(store.getSnapshot().isStale).toBe(true);
    await new Promise((resolve) => setTimeout(resolve, 30));
    watch(store);
    expect(store.getSnapshot().data).toBeUndefined();
  });

  test("disconnect immediately hides a snapshot whose TTL passed while the live stream was healthy", async () => {
    const backend = new Backend();
    const store = new Store("messages", { baseUrl: "https://app.test", fetch: backend.fetch });
    watch(store);
    backend.reads[0]!.resolve("private", { maxAgeMs: 10, version: "1" });
    await until(() => store.getSnapshot().data === "private");
    backend.streams[0]!.send("tako.store.ready");
    await until(() => backend.reads.length === 2);
    backend.reads[1]!.resolve("private", { maxAgeMs: 10, version: "1" });
    await until(() => store.getSnapshot().isStale === false);
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(store.getSnapshot().data).toBe("private");
    backend.streams[0]!.close();
    await until(() => store.getSnapshot().status === "offline");
    expect(store.getSnapshot().data).toBeUndefined();
  });

  test("a logout broadcast invalidates inactive snapshots after their last subscription closes", async () => {
    const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
    if (typeof window === "undefined") {
      Object.defineProperty(globalThis, "window", {
        value: globalThis,
        configurable: true,
        writable: true,
      });
    }
    const peer = new BroadcastChannel("tako-stores-cache");
    try {
      const backend = new Backend();
      const cache = new MemoryCache();
      const store = new Store("messages", {
        baseUrl: "https://app.test",
        cacheScope: "other-tab",
        cache,
        fetch: backend.fetch,
      });
      const reading = store.refresh();
      backend.reads[0]!.resolve("private", { maxAgeMs: 10_000, version: "1" });
      await reading;
      expect(store.getSnapshot().data).toBe("private");
      // No active subscribers/requests remain in this tab when another tab logs out.
      peer.postMessage({ type: "clear", scope: "other-tab" });
      await until(() => store.getSnapshot().data === undefined);
      await until(() => cache.entries.size === 0);
    } finally {
      peer.close();
      if (previousWindow) Object.defineProperty(globalThis, "window", previousWindow);
      else Reflect.deleteProperty(globalThis, "window");
    }
  });

  test.each(["expired", "future", "version", "oversized"])(
    "ignores a %s persisted record",
    async (kind) => {
      const cache = new MemoryCache();
      const backend = new Backend();
      const options = {
        baseUrl: "https://app.test",
        cacheScope: `malformed-${kind}`,
        cache,
        fetch: backend.fetch,
      };
      const seed = new Store("messages", options);
      const reading = seed.refresh();
      backend.reads[0]!.resolve("seed", { maxAgeMs: 10_000, version: "1" });
      await reading;
      const [key, entry] = [...cache.entries][0]!;
      if (kind === "expired") entry.expiresAt = Date.now() - 1;
      if (kind === "future") entry.savedAt = Date.now() + 60_000;
      if (kind === "version") entry.version = "old";
      if (kind === "oversized") entry.data = "x".repeat(1_048_577);
      cache.entries.set(key, entry);
      const offline = new Store("messages", options);
      watch(offline);
      backend.reads[1]!.reject(new Error("offline"));
      await until(() => offline.getSnapshot().status === "error");
      expect(offline.getSnapshot().data).toBeUndefined();
      await until(() => cache.entries.size === 0);
    },
  );

  test("explicit cache clear reports a deletion failure after stopping the reader", async () => {
    const cache = new MemoryCache();
    const clear = cache.clear.bind(cache);
    let fail = true;
    cache.clear = async (scope) => {
      if (fail) {
        fail = false;
        throw new Error("cannot delete persisted data");
      }
      await clear(scope);
    };
    const backend = new Backend();
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      cacheScope: "deletion-failure",
      cache,
      fetch: backend.fetch,
    });
    watch(store);
    const clearing = clearStoreCache("deletion-failure");
    expect(backend.reads[0]!.init?.signal?.aborted).toBe(true);
    await expect(clearing).rejects.toThrow("cannot delete persisted data");
    expect(store.getSnapshot().data).toBeUndefined();
    await clearStoreCache("deletion-failure");
  });

  test("storage failures do not turn a successful online read into an error", async () => {
    const cache: StoreCache = {
      read: async () => {
        throw new Error("storage unavailable");
      },
      write: async () => {
        throw new Error("quota exceeded");
      },
      remove: async () => {},
      clear: async () => {},
    };
    const backend = new Backend();
    const store = new Store("messages", {
      baseUrl: "https://app.test",
      cacheScope: "alice",
      cache,
      fetch: backend.fetch,
    });
    watch(store);
    backend.reads[0]!.resolve("online", { maxAgeMs: 10_000, version: "1" });
    await until(() => store.getSnapshot().data === "online");
    expect(store.getSnapshot().status).toBe("ready");
    expect(store.getSnapshot().error).toBeNull();
  });

  test("canonical runtime identity includes credentials while persisted identity does not", () => {
    const first = storeIdentity("messages", {
      baseUrl: "https://app.test",
      params: { b: 2, a: 1 },
      authorization: "one",
      cacheScope: "alice",
    });
    const reordered = storeIdentity("messages", {
      baseUrl: "https://app.test/other",
      params: { a: 1, b: 2 },
      authorization: "one",
      cacheScope: "alice",
    });
    expect(first).toBe(reordered);
    expect(first).not.toBe(
      storeIdentity("messages", {
        baseUrl: "https://app.test",
        params: { a: 1, b: 2 },
        authorization: "two",
        cacheScope: "alice",
      }),
    );
    const circular: Record<string, unknown> = {};
    circular["self"] = circular;
    expect(() => new Store("messages", { params: circular })).toThrow("JSON-serializable");
  });
});

async function withIndexedDB(run: () => Promise<void>): Promise<void> {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "indexedDB");
  Object.defineProperty(globalThis, "indexedDB", {
    value: indexedDB,
    configurable: true,
    writable: true,
  });
  try {
    await defaultStoreCache.clear();
    await run();
  } finally {
    await defaultStoreCache.clear();
    if (previous) Object.defineProperty(globalThis, "indexedDB", previous);
    else Reflect.deleteProperty(globalThis, "indexedDB");
  }
}

describe("default IndexedDB Store cache", () => {
  test("restores an authorized backend response without an injected cache adapter", async () => {
    await withIndexedDB(async () => {
      const backend = new Backend();
      const options = { baseUrl: "https://idb.test", cacheScope: "idb-user", fetch: backend.fetch };
      const store = new Store("messages", options);
      const reading = store.refresh();
      backend.reads[0]!.resolve({ text: "persisted" }, { maxAgeMs: 10_000, version: "1" });
      await reading;
      const key = JSON.stringify(["https://idb.test", "messages", {}, "idb-user", "1"]);
      expect((await defaultStoreCache.read(key))?.data).toEqual({ text: "persisted" });
      const offline = new Store("messages", options);
      const offlineRead = offline.refresh();
      backend.reads[1]!.reject(new Error("offline"));
      await offlineRead;
      await until(() => offline.getSnapshot().status === "offline");
      expect(offline.getSnapshot().data).toEqual({ text: "persisted" });
      await clearStoreCache("idb-user");
      expect(await defaultStoreCache.read(key)).toBeUndefined();
      expect(offline.getSnapshot().data).toBeUndefined();
    });
  });

  test("bounds entry count, total bytes, individual bytes and removes expired records", async () => {
    await withIndexedDB(async () => {
      const now = Date.now();
      const entry = (key: string, i: number, data: unknown = "small"): StoreCacheEntry => ({
        key,
        scope: "bounded",
        version: "1",
        savedAt: now - 100 + i,
        expiresAt: now + 60_000,
        data,
      });
      for (let i = 0; i < 66; i++) await defaultStoreCache.write(entry(`count-${i}`, i));
      expect(await defaultStoreCache.read("count-0")).toBeUndefined();
      expect(await defaultStoreCache.read("count-1")).toBeUndefined();
      expect((await defaultStoreCache.read("count-65"))?.data).toBe("small");
      await defaultStoreCache.clear();
      for (let i = 0; i < 17; i++)
        await defaultStoreCache.write(entry(`bytes-${i}`, i, "x".repeat(1_000_000)));
      expect(await defaultStoreCache.read("bytes-0")).toBeUndefined();
      expect((await defaultStoreCache.read("bytes-16"))?.data).toHaveLength(1_000_000);
      await defaultStoreCache.write(entry("too-big", 0, "x".repeat(1_048_577)));
      expect(await defaultStoreCache.read("too-big")).toBeUndefined();
      await defaultStoreCache.write({ ...entry("expired", 0), expiresAt: now - 1 });
      expect(await defaultStoreCache.read("expired")).toBeUndefined();
    });
  });

  test("a scoped clear preserves another user's records", async () => {
    await withIndexedDB(async () => {
      const entry = {
        version: "1",
        savedAt: Date.now(),
        expiresAt: Date.now() + 10_000,
        data: "private",
      };
      await defaultStoreCache.write({ ...entry, key: "alice", scope: "alice" });
      await defaultStoreCache.write({ ...entry, key: "bob", scope: "bob" });
      await defaultStoreCache.clear("alice");
      expect(await defaultStoreCache.read("alice")).toBeUndefined();
      expect((await defaultStoreCache.read("bob"))?.data).toBe("private");
    });
  });
});
