---
title: Cache
description: "Cache JSON on your Tako server with per-key TTLs, bounded disk and memory use, and keys scoped to the reader."
---

`tako.cache` is the JavaScript and TypeScript client for Tako's server-owned JSON cache. Use it for values that can be recomputed, such as an expensive query result or an external API response. App code calls the runtime over its internal socket; it needs no database driver.

Go and Rust currently have no equivalent SDK cache API. Use an application-owned cache library for those backends, or a [KV store](/kv/), which has `get`, `put`, and `delete` with a TTL in all three SDKs; the snippets below specifically demonstrate `tako.cache`.

## Read and populate a cache entry

```ts
import { tako } from "tako.sh";

type Product = { id: string; title: string };

async function readProduct(id: string): Promise<Product> {
  const key = `product:${id}`;
  const cached = await tako.cache.get<Product>(key);
  if (cached !== undefined) return cached;

  const product = await loadProduct(id); // Your database or API helper.
  await tako.cache.put(key, product, { ttl: 60_000 });
  return product;
}
```

`get<T>` returns `undefined` for a missing or expired key. `put` overwrites an existing entry and requires a finite positive TTL in milliseconds. Values use JSON serialization; use plain JSON data rather than class instances or values that cannot serialize. Keys must be non-empty.

After a write, explicitly delete stale cached data:

```ts
await tako.cache.delete(`product:${id}`);
```

There is no automatic connection between your database writes and this cache. Concurrent cache misses can run the loader more than once; the read-then-populate example does not provide a distributed lock.

## Share a value between apps

Use `{ shared: true }` on every operation that addresses a shared key. For example, a worker can fill a product catalog entry and an API can read it:

```ts
// Catalog worker
await tako.cache.put("catalog:v1:42", product, { ttl: 60_000, shared: true });
// API app on the same server, in the same environment
const cached = await tako.cache.get<Product>("catalog:v1:42", { shared: true });
await tako.cache.delete("catalog:v1:42", { shared: true });
```

Without `shared: true`, the same key is private to the app. There is no fallback between the two stores. Any app in that server/environment can read, overwrite, or delete a shared entry. The shared store has its own 64 MiB budget across those apps and survives individual app restores. Locally, projects share only when using the same Tako home and environment.

## Persistence and isolation

`tako-server` stores private entries in `data/tako/cache.redb` and shared entries under the server's `shared-cache/` directory. Both are outside the app-owned data directory and app backups. Workers and instances of the same app/environment on one server share entries across releases and restarts. Different environments and servers have independent caches, so a value written on one server is a miss on the others; keep values every server must read, such as login challenges and sessions, in a [KV store](/kv/). `tako dev` and `tako run` use the same stores for the selected local environment.

Each store has a 64 MiB budget for JSON, keys, and per-entry overhead. Keys are limited to 1 KiB of UTF-8 and values to 256 KiB of serialized JSON. Writes reclaim expired entries even if nobody reads those keys; under pressure the soonest-expiring entries go first. Always handle a miss before the TTL. redb reuses freed pages, so the file can stay at its high-water mark. The byte budget is not an exact filesystem-size limit.

The server keeps up to eight write handles (512 KiB page cache each) and 64 read handles (64 KiB each), for 8 MiB of configured page cache across apps. Read handles are kept per store, so a round-robin read workload across up to 64 apps does not reopen a database per request; less active stores give up their handle first. Other memory, such as requests in flight and filesystem caching, is additional. A write to a store outside the eight most recent reopens its existing file on demand.

Include the tenant, user, data version, and other authorization-relevant inputs in keys when results differ by reader. Authorize the request before returning private cached data. Do not treat a cache key as an access check.

Native processes, container workers with an internal socket, and Compose services with `x-tako: { sdk: true }` can use the cache. Standalone HTTP container releases have no internal socket. An app data mount alone does not grant access. Browser code cannot use this cache.

Upgrade the server and SDK together. The new cache starts cold. After every old SDK process has stopped, the old `data/tako/cache.sqlite` and its `-wal` and `-shm` companions can be removed to reclaim their disk space.

## Choose the right cache

- Use `tako.cache` for disposable backend JSON with a TTL that each server can recompute on its own.
- Use a [KV store](/kv/) for values every server of the network must see, such as challenges, one-time tokens, sessions, and flags. It has the same `get`, `put`, and `delete` of JSON, with an optional TTL; it writes to disk before acknowledging, never evicts, is backed up like a database, and works from JavaScript, Go, and Rust.
- Use [Stores](/stores/#opt-into-cached-offline-reads) for authorized UI reads with optional IndexedDB persistence and explicit invalidation.
- Use your application database for canonical records.
- Use [Storage](/storage/) for uploaded files.

## Verify

Run through `tako dev` or `tako run`, read a missing key, populate it, confirm a second read returns the value, and confirm deletion and expiry return `undefined`. For private data, verify that different reader scopes cannot reuse each other's cached values.
