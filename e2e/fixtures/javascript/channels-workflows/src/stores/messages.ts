import { createClient, type Client } from "@tursodatabase/serverless/compat";
import { defineStore, tako } from "tako.sh";

let database: Promise<Client> | undefined;

/** The `messages` database binding, created on first use. */
function db(): Promise<Client> {
  database ??= (async () => {
    const binding = tako.db.messages;
    if (!binding) throw new Error("No database is bound as `messages`");
    const client = createClient(binding);
    await client.execute(
      "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, chat_id TEXT NOT NULL, message TEXT NOT NULL)",
    );
    return client;
  })().catch((error: unknown) => {
    database = undefined;
    throw error;
  });
  return database;
}

export async function appendMessage(chatId: string, message: string): Promise<void> {
  await (
    await db()
  ).execute({
    sql: "INSERT INTO messages (chat_id, message) VALUES (?, ?)",
    args: [chatId, message],
  });
}

export default defineStore("messages", {
  paramsSchema: (t) => t.Object({ chat: t.Object({ id: t.String() }) }),
  auth: {
    verify: ({ header }) => (header?.value === "e2e" ? { subject: "e2e-user" } : false),
  },
  offline: { maxAgeMs: 60_000 },
  load: async ({ params, subject }) => {
    const result = await (
      await db()
    ).execute({
      sql: "SELECT id, message FROM messages WHERE chat_id = ? ORDER BY id",
      args: [params.chat.id],
    });
    return {
      chatId: params.chat.id,
      subject,
      messages: result.rows.map((row) => ({
        id: Number(row["id"]),
        message: String(row["message"]),
      })),
    };
  },
});
