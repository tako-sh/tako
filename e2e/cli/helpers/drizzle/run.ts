/**
 * Runs under `tako run`: applies Drizzle migrations to the bound database
 * through `@libsql/client`, then reads and writes through the ORM.
 */
import { createClient } from "@libsql/client";
import { eq, sql } from "drizzle-orm";
import { drizzle } from "drizzle-orm/libsql";
import { migrate } from "drizzle-orm/libsql/migrator";
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

const client = createClient(main);
const db = drizzle(client);
await migrate(db, { migrationsFolder: new URL("./migrations", import.meta.url).pathname });

const [author] = await db.insert(authors).values({ name: "Ada" }).returning();
await db.insert(posts).values([
  { authorId: author!.id, title: "Engines", published: true },
  { authorId: author!.id, title: "Notes" },
]);
const published = await db.select().from(posts).where(eq(posts.published, true));
const applied = await client.execute("SELECT count(*) AS n FROM __drizzle_migrations");
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
