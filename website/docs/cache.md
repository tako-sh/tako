---
title: Cache
description: "Cache server-side JSON values with Tako's SQLite-backed cache, choose TTLs and tenant-safe keys, and understand per-server persistence limits."
---

`tako.cache` is a server-side JSON cache in the JavaScript and TypeScript SDK. Use it for values that can be recomputed, such as an expensive query result or an external API response. It requires a Tako-managed runtime with `TAKO_DATA_DIR` set and Bun or Node.js with `node:sqlite` support.

Go and Rust currently have no equivalent SDK cache API. Use an application-owned cache library for those backends; the snippets below specifically demonstrate `tako.cache`.

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

## Persistence and isolation

Entries live in Tako-managed `cache.sqlite`, separately from your app-owned data directory. Processes using the same cache path share the SQLite file. The cache is local to each server and is not replicated through `postgres_url`. App data backups omit it.

Include the tenant, user, data version, and other authorization-relevant inputs in keys when results differ by reader. Authorize the request before returning private cached data. Do not treat a cache key as an access check.

Container HTTP instances do not receive `TAKO_DATA_DIR` in v0, so this helper is not available there by default. Browser code cannot use this server cache.

## Choose the right cache

- Use `tako.cache` for disposable backend JSON with a TTL.
- Use [Stores](/stores/#opt-into-cached-offline-reads) for authorized UI reads with optional IndexedDB persistence and explicit invalidation.
- Use your application database for canonical records.
- Use [Storage](/storage/) for uploaded files.

## Verify

Run through `tako dev` or `tako run`, read a missing key, populate it, confirm a second read returns the value, and confirm deletion and expiry return `undefined`. For private data, verify that different reader scopes cannot reuse each other's cached values.
