import { Database } from "bun:sqlite";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { defineStore, tako } from "tako.sh";

let database: Database | undefined;

function db(): Database {
  if (!database) {
    mkdirSync(tako.dataDir, { recursive: true });
    database = new Database(join(tako.dataDir, "store-messages.sqlite"), { create: true });
    database.exec("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;");
    database.exec(
      "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, chat_id TEXT NOT NULL, message TEXT NOT NULL)",
    );
  }
  return database;
}

export function appendMessage(chatId: string, message: string): void {
  db().query("INSERT INTO messages (chat_id, message) VALUES (?, ?)").run(chatId, message);
}

export default defineStore("messages", {
  paramsSchema: (t) => t.Object({ chat: t.Object({ id: t.String() }) }),
  auth: {
    verify: ({ header }) => (header?.value === "e2e" ? { subject: "e2e-user" } : false),
  },
  offline: { maxAgeMs: 60_000 },
  load: ({ params, subject }) => ({
    chatId: params.chat.id,
    subject,
    messages: db()
      .query<{ id: number; message: string }, [string]>(
        "SELECT id, message FROM messages WHERE chat_id = ? ORDER BY id",
      )
      .all(params.chat.id),
  }),
});
