---
title: "Stateful Apps on Tako: SQLite and Uploads That Survive Deploys"
date: "2026-04-14T10:00"
description: "Tako gives each app persistent storage for SQLite databases, uploads, and queue data, so rolling deploys do not wipe local state."
image: a6572581d903
---

The first thing most side projects outgrow isn't their server — it's the assumption that they don't need persistent storage.

You start with a stateless API. Static responses, external auth, everything in memory. Then someone asks for user preferences, or you want to store uploaded avatars, or you need to track some simple counts. And suddenly you're pricing out managed PostgreSQL.

On a $5 VPS, that's your entire hosting budget.

Tako gives every app a persistent data directory that outlives deploys and rolling restarts. SQLite, file uploads, queue data — anything that lives in a file works there, without an external service.

## The persistent data directory

Each app gets a directory that Tako owns and preserves. You do not provision it, mount it, or pass it through deploy config. Tako creates it automatically in both dev and production:

| Environment   | Path                                                  |
| ------------- | ----------------------------------------------------- |
| `tako dev`    | `.tako/data/app/` (inside your project)               |
| `tako deploy` | `/opt/tako/data/apps/{app}/data/app/` (on the server) |

That directory persists across:

- **Deploys** — rolling restarts swap the release directory, not the data directory
- **Server restarts** and `tako-server` upgrades
- **Scale-to-zero** idle cycles — the directory is on disk, not in process memory

It's only cleaned up when you explicitly delete the app.

Your code reaches it through `tako.dataDir` from `tako.sh`. Under the hood, Tako sets `TAKO_DATA_DIR`; most app code should use the typed helper instead of reading the environment variable directly.

## SQLite without a managed database

SQLite is underrated for side projects. It's fast, reliable, needs zero infrastructure, and scales comfortably to millions of rows on any modern VPS. The only catch is that most deploy tools don't give you a reliable place to put the file.

Tako's data directory is that place.

```typescript
import { Database } from "bun:sqlite";
import { join } from "path";
import { tako } from "tako.sh";

const db = new Database(join(tako.dataDir, "app.db"));
db.run(`
  CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
  )
`);

export default async function fetch(req: Request) {
  if (req.method === "POST" && new URL(req.url).pathname === "/notes") {
    const { body } = await req.json();
    db.run("INSERT INTO notes (body) VALUES (?)", [body]);
    return new Response("ok");
  }
  const notes = db.query("SELECT * FROM notes ORDER BY created_at DESC").all();
  return Response.json(notes);
}
```

The database file lives in the persistent app data directory. Deploy a new version and the release directory swaps, but the data directory stays put. Your rows are exactly where you left them.

## File uploads

The same pattern applies to any file-based storage:

```typescript
import { writeFile, mkdir } from "fs/promises";
import { join } from "path";
import { tako } from "tako.sh";

const uploadsDir = join(tako.dataDir, "uploads");
await mkdir(uploadsDir, { recursive: true });

export default async function fetch(req: Request) {
  if (req.method === "POST" && new URL(req.url).pathname === "/upload") {
    const formData = await req.formData();
    const file = formData.get("file") as File;
    await writeFile(join(uploadsDir, file.name), Buffer.from(await file.arrayBuffer()));
    return Response.json({ path: `/files/${file.name}` });
  }
  // serve files from uploadsDir...
}
```

Uploaded files persist across deploys. New releases start, old ones drain — the files are untouched.

## Dev/prod parity

In development, `tako dev` uses `.tako/data/app/` inside your project directory. Same `tako.dataDir` helper, same code path, different location. No mocking, no special cases.

If you want a clean local state, delete `.tako/data/app/` — the same reasoning applies in production: the data persists until you intentionally clear it.

Run `tako generate` and the generated `tako.d.ts` keeps `tako.dataDir` and your secrets typed, so your editor knows what's available.

## Where this doesn't replace managed infrastructure

Persistent app storage is a single-server guarantee. If you're running the same app across multiple servers, each server has its own independent data directory — they don't sync. For multi-server setups you'll want either:

- An external database (Turso, PlanetScale, Neon)
- SQLite replication (LiteFS or Litestream) pointed at the app data directory
- Architecture that avoids shared mutable state

For most side projects on a single server, none of that is necessary. A SQLite file in the app data directory handles the load, survives the deploys, and costs nothing extra.

## Try it

The data directory is available automatically on every deploy — no configuration required.

```bash
tako deploy
```

See the [deployment docs](/docs/deployment) for the full setup, the [development guide](/docs/development) for how data directories behave locally, and the [CLI reference](/docs/cli) for app lifecycle commands including `tako app delete`.
