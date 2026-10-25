/** A persisted, previously authorized Store response. Credentials are never stored. */
export interface StoreCacheEntry {
  key: string;
  scope: string;
  version: string;
  savedAt: number;
  expiresAt: number;
  data: unknown;
}

/** Optional persistence adapter. Failures never prevent online Store reads. */
export interface StoreCache {
  read(key: string): Promise<StoreCacheEntry | undefined>;
  write(entry: StoreCacheEntry): Promise<void>;
  remove(key: string): Promise<void>;
  clear(scope?: string): Promise<void>;
}

export const STORE_CACHE_MAX_BYTES = 1_048_576;
export const STORE_CACHE_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;
const MAX_CACHE_ENTRIES = 64;
const MAX_CACHE_TOTAL_BYTES = 16 * STORE_CACHE_MAX_BYTES;
const DATABASE_NAME = "tako-stores";
const TABLE_NAME = "snapshots";

function entrySize(entry: StoreCacheEntry): number {
  return new TextEncoder().encode(JSON.stringify(entry)).byteLength;
}

class IndexedDBStoreCache implements StoreCache {
  private connection: Promise<IDBDatabase | undefined> | undefined;

  private database(): Promise<IDBDatabase | undefined> {
    if (typeof globalThis.indexedDB === "undefined") return Promise.resolve(undefined);
    this.connection ??= new Promise<IDBDatabase | undefined>((resolve, reject) => {
      let failed = false;
      const request = globalThis.indexedDB.open(DATABASE_NAME, 1);
      request.onupgradeneeded = () => {
        request.result.createObjectStore(TABLE_NAME, { keyPath: "key" });
      };
      request.onsuccess = () => {
        const database = request.result;
        if (failed) {
          database.close();
          return;
        }
        database.onversionchange = () => {
          database.close();
          this.connection = undefined;
        };
        resolve(database);
      };
      request.onerror = () => {
        failed = true;
        reject(request.error);
      };
      request.onblocked = () => {
        failed = true;
        reject(new Error("Store cache database is blocked."));
      };
    }).catch((error: unknown) => {
      this.connection = undefined;
      throw error;
    });
    return this.connection;
  }

  async read(key: string): Promise<StoreCacheEntry | undefined> {
    const database = await this.database();
    if (!database) return undefined;
    return new Promise((resolve, reject) => {
      const transaction = database.transaction(TABLE_NAME, "readonly");
      const request = transaction.objectStore(TABLE_NAME).get(key);
      request.onsuccess = () => resolve(request.result as StoreCacheEntry | undefined);
      request.onerror = () => reject(request.error);
    });
  }

  async write(entry: StoreCacheEntry): Promise<void> {
    if (entrySize(entry) > STORE_CACHE_MAX_BYTES) return;
    const database = await this.database();
    if (!database) return;
    await new Promise<void>((resolve, reject) => {
      const transaction = database.transaction(TABLE_NAME, "readwrite");
      const table = transaction.objectStore(TABLE_NAME);
      table.put(entry);
      const request = table.getAll();
      request.onsuccess = () => {
        const entries = (request.result as StoreCacheEntry[]).sort((a, b) => b.savedAt - a.savedAt);
        let bytes = 0;
        let count = 0;
        for (const value of entries) {
          let size: number;
          try {
            size = entrySize(value);
          } catch {
            table.delete(value.key);
            continue;
          }
          if (
            value.expiresAt <= Date.now() ||
            size > STORE_CACHE_MAX_BYTES ||
            count >= MAX_CACHE_ENTRIES ||
            bytes + size > MAX_CACHE_TOTAL_BYTES
          ) {
            table.delete(value.key);
          } else {
            bytes += size;
            count++;
          }
        }
      };
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    });
  }

  async remove(key: string): Promise<void> {
    const database = await this.database();
    if (!database) return;
    await new Promise<void>((resolve, reject) => {
      const transaction = database.transaction(TABLE_NAME, "readwrite");
      transaction.objectStore(TABLE_NAME).delete(key);
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    });
  }

  async clear(scope?: string): Promise<void> {
    const database = await this.database();
    if (!database) return;
    await new Promise<void>((resolve, reject) => {
      const transaction = database.transaction(TABLE_NAME, "readwrite");
      const table = transaction.objectStore(TABLE_NAME);
      if (scope === undefined) {
        table.clear();
      } else {
        const request = table.openCursor();
        request.onsuccess = () => {
          const cursor = request.result;
          if (!cursor) return;
          if ((cursor.value as StoreCacheEntry).scope === scope) cursor.delete();
          cursor.continue();
        };
      }
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    });
  }
}

export const defaultStoreCache: StoreCache = new IndexedDBStoreCache();

const operations = new WeakMap<StoreCache, Promise<void>>();

/** @internal Serialize writes/deletes so logout cannot be followed by an old write. */
export function mutateStoreCache(cache: StoreCache, operation: () => Promise<void>): Promise<void> {
  const pending = (operations.get(cache) ?? Promise.resolve()).catch(() => {}).then(operation);
  operations.set(
    cache,
    pending.catch(() => {}),
  );
  return pending;
}

interface CacheConsumer {
  scope: string | undefined;
  cache: StoreCache;
  clear(): void;
}
const consumers = new Set<CacheConsumer>();
const adapters = new Set<StoreCache>([defaultStoreCache]);
let revision = 0;
let allScopesRevision = 0;
const scopeRevisions = new Map<string, number>();
let broadcasts: BroadcastChannel | undefined;

/** @internal Inactive snapshots must also become unreadable after logout. */
export function storeCacheRevision(scope: string | undefined): number {
  return Math.max(allScopesRevision, scope === undefined ? 0 : (scopeRevisions.get(scope) ?? 0));
}

function cacheBroadcasts(): BroadcastChannel | undefined {
  if (typeof window === "undefined" || typeof globalThis.BroadcastChannel === "undefined")
    return undefined;
  if (!broadcasts) {
    broadcasts = new BroadcastChannel("tako-stores-cache");
    (broadcasts as unknown as { unref?: () => void }).unref?.();
    broadcasts.onmessage = (event: MessageEvent<unknown>) => {
      const value = event.data as { type?: unknown; scope?: unknown } | null;
      if (
        value?.type === "clear" &&
        (value.scope === undefined || typeof value.scope === "string")
      ) {
        void clearCaches(value.scope, false).catch(() => {});
      }
    };
  }
  return broadcasts;
}

/** @internal A cache clear also stops existing readers from repopulating it. */
export function registerStoreCacheConsumer(consumer: CacheConsumer): () => void {
  consumers.add(consumer);
  adapters.add(consumer.cache);
  cacheBroadcasts();
  return () => {
    consumers.delete(consumer);
  };
}

async function clearCaches(scope: string | undefined, broadcast: boolean): Promise<void> {
  revision++;
  if (scope === undefined) allScopesRevision = revision;
  else scopeRevisions.set(scope, revision);
  for (const consumer of consumers) {
    if (scope === undefined || consumer.scope === scope) {
      consumer.clear();
    }
  }
  if (broadcast) cacheBroadcasts()?.postMessage({ type: "clear", scope });
  const results = await Promise.allSettled(
    [...adapters].map((cache) => mutateStoreCache(cache, () => cache.clear(scope))),
  );
  for (const result of results) {
    if (result.status === "rejected") throw result.reason;
  }
}

/**
 * Remove persisted Store data and stop active readers for a user/cache scope.
 * Call during logout, then change the hook's credentials/scope or disable it.
 * Omit scope to clear all Store caches. Other open tabs receive the clear too.
 */
export function clearStoreCache(scope?: string): Promise<void> {
  return clearCaches(scope, true);
}
