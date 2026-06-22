interface CachePutOptions {
  /** Time to live in milliseconds. */
  ttl: number;
}

interface CacheRow {
  value: string;
  expires_at: number;
}

interface SqliteStatement<T = unknown> {
  get?: (...params: unknown[]) => T | undefined;
  run: (...params: unknown[]) => unknown;
}

interface SqliteDatabase {
  exec(sql: string): unknown;
  prepare<T = unknown>(sql: string): SqliteStatement<T>;
}

type SqliteDatabaseConstructor = new (path: string) => SqliteDatabase;

const databasePromises = new Map<string, Promise<SqliteDatabase>>();

const runtimeImport = new Function("specifier", "return import(specifier)") as <T>(
  specifier: string,
) => Promise<T>;

async function getCachedValue<T>(key: string): Promise<T | undefined> {
  validateCacheKey(key);

  const db = await openCacheDatabase();
  const now = Date.now();
  const row = db
    .prepare<CacheRow>("SELECT value, expires_at FROM cache_entries WHERE key = ?")
    .get?.(key);

  if (!row) {
    return undefined;
  }
  if (row.expires_at <= now) {
    db.prepare("DELETE FROM cache_entries WHERE key = ?").run(key);
    return undefined;
  }
  return JSON.parse(row.value) as T;
}

async function putCachedValue<T>(key: string, value: T, options: CachePutOptions): Promise<void> {
  validateCacheKey(key);
  validateTtl(options.ttl);

  const encoded = JSON.stringify(value);
  if (encoded === undefined) {
    throw new Error("Cache value must be JSON-serializable.");
  }

  const db = await openCacheDatabase();
  const now = Date.now();
  const expiresAt = now + options.ttl;
  db.prepare(
    `INSERT INTO cache_entries (key, value, expires_at, updated_at)
     VALUES (?, ?, ?, ?)
     ON CONFLICT(key) DO UPDATE SET
       value = excluded.value,
       expires_at = excluded.expires_at,
       updated_at = excluded.updated_at`,
  ).run(key, encoded, expiresAt, now);
}

async function deleteCachedValue(key: string): Promise<void> {
  validateCacheKey(key);
  const db = await openCacheDatabase();
  db.prepare("DELETE FROM cache_entries WHERE key = ?").run(key);
}

/**
 * Server-side JSON cache backed by Tako-managed SQLite storage.
 *
 * Available only inside a Tako-managed runtime with `TAKO_DATA_DIR` set.
 */
export const cache = Object.freeze({
  /**
   * Read a cache entry.
   *
   * @param key - Cache key.
   * @returns The stored value, or `undefined` when missing or expired.
   */
  get<T>(key: string): Promise<T | undefined> {
    return getCachedValue<T>(key);
  },
  /**
   * Store a JSON-serializable cache entry.
   *
   * @param key - Cache key.
   * @param value - JSON-serializable value.
   * @param options - Cache options.
   */
  put<T>(key: string, value: T, options: { ttl: number }): Promise<void> {
    return putCachedValue(key, value, options);
  },
  /**
   * Delete one cache entry.
   *
   * @param key - Cache key.
   */
  delete(key: string): Promise<void> {
    return deleteCachedValue(key);
  },
});

async function openCacheDatabase(): Promise<SqliteDatabase> {
  const path = await cacheDatabasePath();
  let databasePromise = databasePromises.get(path);
  if (!databasePromise) {
    databasePromise = (async () => {
      const Database = await loadSqliteDatabase();
      const db = new Database(path);
      db.exec("PRAGMA journal_mode = WAL");
      db.exec("PRAGMA busy_timeout = 5000");
      db.exec(`
        CREATE TABLE IF NOT EXISTS cache_entries (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL,
          expires_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL
        ) STRICT
      `);
      db.exec(
        "CREATE INDEX IF NOT EXISTS cache_entries_expires_at_idx ON cache_entries (expires_at)",
      );
      db.prepare("DELETE FROM cache_entries WHERE expires_at <= ?").run(Date.now());
      return db;
    })();
    databasePromises.set(path, databasePromise);
  }
  return databasePromise;
}

async function cacheDatabasePath(): Promise<string> {
  const dataDir = typeof process !== "undefined" ? process.env["TAKO_DATA_DIR"] : undefined;
  if (!dataDir) {
    throw new Error("TAKO_DATA_DIR is not set. Run this cache helper inside Tako.");
  }

  const [{ dirname, join }, { mkdir }] = await Promise.all([
    runtimeImport<typeof import("node:path")>("node:path"),
    runtimeImport<typeof import("node:fs/promises")>("node:fs/promises"),
  ]);
  const takoDataDir = join(dirname(dataDir), "tako");
  await mkdir(takoDataDir, { recursive: true });
  return join(takoDataDir, "cache.sqlite");
}

async function loadSqliteDatabase(): Promise<SqliteDatabaseConstructor> {
  if (typeof Bun !== "undefined") {
    const sqlite = await runtimeImport<{ Database: SqliteDatabaseConstructor }>("bun:sqlite");
    return sqlite.Database;
  }

  try {
    const sqlite = await runtimeImport<{ DatabaseSync: SqliteDatabaseConstructor }>("node:sqlite");
    return sqlite.DatabaseSync;
  } catch (error) {
    throw new Error("Tako cache requires Bun or a Node.js runtime with node:sqlite.", {
      cause: error,
    });
  }
}

function validateCacheKey(key: string): void {
  if (key.trim() === "") {
    throw new Error("Cache key cannot be empty.");
  }
}

function validateTtl(ttl: number): void {
  if (!Number.isFinite(ttl) || ttl <= 0) {
    throw new Error("Cache ttl must be a positive number of milliseconds.");
  }
}
