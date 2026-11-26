/**
 * Runs under `tako run`: applies Drizzle migrations to the bound database
 * through the Hrana v2 HTTP stream, then reads and writes through the ORM.
 */
import { openHttp } from "@libsql/hrana-client";
import { eq, sql } from "drizzle-orm";
import { drizzle } from "drizzle-orm/sqlite-proxy";
import { migrate } from "drizzle-orm/sqlite-proxy/migrator";
import { integer, sqliteTable, text } from "drizzle-orm/sqlite-core";

const authors = sqliteTable("authors", {
  id: integer("id").primaryKey({ autoIncrement: true }),
  name: text("name").notNull(),
});
const posts = sqliteTable("posts", {
  id: integer("id").primaryKey({ autoIncrement: true }),
  authorId: integer("author_id")
    .notNull()
    .references(() => authors.id, { onDelete: "cascade" }),
  title: text("title").notNull(),
  published: integer("published", { mode: "boolean" }).notNull().default(false),
});

const envelope = JSON.parse(process.env["TAKO_BOOTSTRAP_DATA"] ?? "{}") as {
  databases?: Record<string, { url: string; authToken: string }>;
};
const main = envelope.databases?.["drizzle"];
if (!main) throw new Error("no database is bound as drizzle");

const client = openHttp(main.url, main.authToken, undefined, undefined, 2);
const stream = client.openStream();
const db = drizzle(async (sql, args, method) => {
  const result = await stream.query([sql, args]);
  const rows = result.rows.map((row) => Array.from(row));
  return { rows: method === "get" ? rows[0] ?? [] : rows };
});
await migrate(db, async (statements) => {
  await stream.run("BEGIN IMMEDIATE");
  try {
    for (const sql of statements) await stream.run(sql);
    await stream.run("COMMIT");
  } catch (error) {
    await stream.run("ROLLBACK");
    throw error;
  }
}, { migrationsFolder: new URL("./migrations", import.meta.url).pathname });

const [author] = await db.insert(authors).values({ name: "Ada" }).returning();
await db.insert(posts).values([
  { authorId: author!.id, title: "Engines", published: true },
  { authorId: author!.id, title: "Notes" },
]);
const published = await db.select().from(posts).where(eq(posts.published, true));
const applied = await stream.query("SELECT count(*) AS n FROM __drizzle_migrations");
await db
  .transaction(async (tx) => {
    await tx.insert(posts).values({ authorId: author!.id, title: "Draft" });
    tx.rollback();
  })
  .catch(() => {});
const total = await db.select({ n: sql<number>`count(*)` }).from(posts);
// Foreign keys are enforced, so deleting the author cascades.
await db.delete(authors).where(eq(authors.id, author!.id));
const orphans = await db.select({ n: sql<number>`count(*)` }).from(posts);

console.log(
  JSON.stringify({
    published: published.map((post) => post.title),
    migrations: Number(applied.rows[0]?.["n"]),
    total: total[0]?.n,
    afterCascade: orphans[0]?.n,
  }),
);
client.close();
