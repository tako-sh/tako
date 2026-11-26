/**
 * Runs under `tako run` and drives a bound database through the native and libSQL-compatible APIs of
 * the Turso HTTP driver. Prints one JSON report.
 */
import { createClient, LibsqlError } from "@tursodatabase/serverless/compat";
import { connect } from "@tursodatabase/serverless";

import { createDbBag } from "../../../sdk/javascript/src/db";
import { createKvBag } from "../../../sdk/javascript/src/kv";

type Connection = { url: string; authToken: string };

const envelope = JSON.parse(process.env["TAKO_BOOTSTRAP_DATA"] ?? "{}") as {
  databases?: Record<string, Connection>;
  kv?: Record<string, unknown>;
};
const main = envelope.databases?.["main"];
if (!main) throw new Error("no database is bound as main");

const report: Record<string, unknown> = { url: main.url };

const replica = createClient(createDbBag<{main: unknown}>(() => envelope.databases ?? {}).main.readReplica);
report["replicaRead"] = (await replica.execute("SELECT 42 AS n")).rows[0]?.["n"];
report["replicaWrite"] = await code(() => replica.execute("CREATE TABLE forbidden(id)"));
replica.close();
const sessions = createKvBag<{sessions: unknown}>(() => envelope.kv ?? {}).sessions;
await sessions.put("local-read", "example");
report["replicaKV"] = await sessions.readReplica.get("local-read");

async function code(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
    return "ok";
  } catch (error) {
    const e = error as { code?: string; message?: string };
    return e.code ?? e.message ?? String(error);
  }
}

// @tursodatabase/serverless/compat
const libsql = createClient(main);
await libsql.executeMultiple("BEGIN CONCURRENT; SELECT 1; COMMIT;");
report["previousRows"] =
  Number(
    (
      await libsql.execute(
        "SELECT count(*) AS c FROM sqlite_schema WHERE type = 'table' AND name = 'users'",
      )
    ).rows[0]?.["c"],
  ) > 0
    ? Number((await libsql.execute("SELECT count(*) AS c FROM users")).rows[0]?.["c"])
    : 0;
await libsql.execute(
  "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT UNIQUE, data BLOB, score REAL)",
);
const inserted = await libsql.execute({
  sql: "INSERT INTO users (name, data, score) VALUES (?, ?, ?)",
  args: [`ada-${Date.now()}`, new Uint8Array([1, 2, 3]), 1.5],
});
report["insert"] = {
  rowsAffected: inserted.rowsAffected,
  hasRowid: inserted.lastInsertRowid !== undefined,
};
const named = await libsql.execute({
  sql: "SELECT data, score, typeof(data) AS kind FROM users WHERE id = :id",
  args: { id: inserted.lastInsertRowid! },
});
const blob = named.rows[0]?.["data"] as ArrayBuffer;
report["blob"] = Array.from(new Uint8Array(blob));
report["score"] = named.rows[0]?.["score"];
report["kind"] = named.rows[0]?.["kind"];
const integers = connect(main);
const statement = await integers.prepare("SELECT 9007199254740993 AS n");
const row = await statement.safeIntegers().get() as { n: bigint };
report["bigint"] = String(row.n);
await integers.close();
report["batchRollback"] = await code(() =>
  libsql.batch(
    ["INSERT INTO users (name) VALUES ('batch-1')", "INSERT INTO users (name) VALUES ('batch-1')"],
    "write",
  ),
);
report["batchRolledBack"] = Number(
  (await libsql.execute("SELECT count(*) AS c FROM users WHERE name = 'batch-1'")).rows[0]?.["c"],
);
const rollback = await libsql.transaction("write");
await rollback.execute("INSERT INTO users (name) VALUES ('tx-rolled-back')");
await rollback.rollback();
const commit = await libsql.transaction("write");
await commit.execute({ sql: "INSERT INTO users (name) VALUES (?)", args: [`tx-${Date.now()}`] });
await commit.commit();
report["transactions"] = Number(
  (await libsql.execute("SELECT count(*) AS c FROM users WHERE name = 'tx-rolled-back'")).rows[0]?.[
    "c"
  ],
);
await libsql.executeMultiple(
  "CREATE TABLE IF NOT EXISTS events (n INTEGER); INSERT INTO events VALUES (1); INSERT INTO events VALUES (2);",
);
report["constraint"] = await code(() =>
  libsql.execute({
    sql: "INSERT INTO users (id, name) VALUES (?, 'dup')",
    args: [inserted.lastInsertRowid!],
  }),
);
report["syntax"] = await code(() => libsql.execute("SELEC 1"));
report["attach"] = await code(() => libsql.execute("ATTACH DATABASE '/tmp/escape.db' AS escape"));
report["vacuumInto"] = await code(() => libsql.execute("VACUUM INTO '/tmp/escape.db'"));
report["loadExtension"] = await code(() => libsql.execute("SELECT load_extension('x')"));
report["foreignKeys"] = Number((await libsql.execute("PRAGMA foreign_keys")).rows[0]?.[0]);
report["libsqlErrorClass"] = await (async () => {
  try {
    await libsql.execute("SELECT * FROM missing_table");
    return "none";
  } catch (error) {
    return error instanceof LibsqlError ? "LibsqlError" : String(error);
  }
})();
libsql.close();

// @tursodatabase/serverless
const turso = connect(main);
report["tursoGet"] = await (await turso.prepare("SELECT 41 + 1 AS answer")).get();
report["tursoAll"] = (await turso.all("SELECT n FROM events WHERE n > ? ORDER BY n", 0)).length;
report["tursoRun"] = (await turso.run("INSERT INTO events VALUES (?)", 3)).changes;
let streamed = 0;
for await (const _row of turso.iterate(
  "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s LIMIT 5000) SELECT i FROM s",
)) {
  streamed++;
}
report["tursoIterate"] = streamed;
await turso.batch(["INSERT INTO events VALUES (4)", "INSERT INTO events VALUES (5)"]);
const tursoTx = turso.transaction(async () => {
  await turso.run("INSERT INTO events VALUES (6)");
});
await tursoTx();
report["events"] = Number(
  ((await turso.get("SELECT count(*) AS c FROM events")) as { c: number }).c,
);
report["tursoError"] = await code(() =>
  turso.run("INSERT INTO users (id) VALUES (NULL), (NULL) RETURNING nope"),
);
await turso.close();

// A wrong token is rejected.
report["badToken"] = await code(() =>
  createClient({ url: main.url, authToken: "wrong" }).execute("SELECT 1"),
);

console.log(JSON.stringify(report));
