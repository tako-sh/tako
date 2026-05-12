import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

type DbModule = typeof import("../src/server/db");

const DAY_MS = 24 * 60 * 60 * 1000;

let dataDir = "";
let sql: Database;
let db: DbModule;

beforeAll(async () => {
  dataDir = await mkdtemp(path.join(tmpdir(), "tako-demo-db-"));
  process.env.TAKO_DATA_DIR = dataDir;
  db = await import("../src/server/db");
  sql = new Database(path.join(dataDir, "mission.sqlite3"));
});

afterAll(async () => {
  sql.close();
  await rm(dataDir, { recursive: true, force: true });
  delete process.env.TAKO_DATA_DIR;
});

describe("cleanupOldRecords", () => {
  test("cleanup workflow is scheduled daily", async () => {
    const workflow = await import("../src/workflows/cleanup");

    expect(workflow.default.definition.name).toBe("cleanup");
    expect(workflow.default.definition.opts.schedule).toBe("@daily");
  });

  test("deletes supply requests older than three days", () => {
    const now = Date.now();
    const oldRequestId = crypto.randomUUID();
    const recentRequestId = crypto.randomUUID();

    db.ensureBase("cleanup-requests");
    db.createRequest({
      requestId: oldRequestId,
      baseSlug: "cleanup-requests",
      item: "old batteries",
    });
    db.createRequest({
      requestId: recentRequestId,
      baseSlug: "cleanup-requests",
      item: "fresh oxygen",
    });

    setRequestCreatedAt(oldRequestId, now - 4 * DAY_MS);
    setRequestCreatedAt(recentRequestId, now - DAY_MS);

    expect(db.cleanupOldRecords(now)).toEqual({
      cutoff: now - db.RECORD_RETENTION_MS,
      requestsDeleted: 1,
      basesDeleted: 0,
    });
    expect(findRequest(oldRequestId)).toBeNull();
    expect(findRequest(recentRequestId)).toEqual({ request_id: recentRequestId });
  });

  test("deletes stale empty bases but keeps bases with retained requests", () => {
    const now = Date.now();
    const retainedRequestId = crypto.randomUUID();

    db.ensureBase("empty-old-base");
    db.ensureBase("active-old-base");
    db.createRequest({
      requestId: retainedRequestId,
      baseSlug: "active-old-base",
      item: "reactor sealant",
    });

    setBaseCreatedAt("empty-old-base", now - 4 * DAY_MS);
    setBaseCreatedAt("active-old-base", now - 4 * DAY_MS);
    setRequestCreatedAt(retainedRequestId, now - DAY_MS);

    expect(db.cleanupOldRecords(now)).toEqual({
      cutoff: now - db.RECORD_RETENTION_MS,
      requestsDeleted: 0,
      basesDeleted: 1,
    });
    expect(findBase("empty-old-base")).toBeNull();
    expect(findBase("active-old-base")).toEqual({ slug: "active-old-base" });
  });
});

function setRequestCreatedAt(requestId: string, createdAt: number) {
  sql
    .prepare("UPDATE supply_requests SET created_at = ?, updated_at = ? WHERE request_id = ?")
    .run(createdAt, createdAt, requestId);
}

function setBaseCreatedAt(slug: string, createdAt: number) {
  sql.prepare("UPDATE bases SET created_at = ? WHERE slug = ?").run(createdAt, slug);
}

function findRequest(requestId: string): { request_id: string } | null {
  return sql
    .prepare<{ request_id: string }, string>(
      "SELECT request_id FROM supply_requests WHERE request_id = ?",
    )
    .get(requestId);
}

function findBase(slug: string): { slug: string } | null {
  return sql.prepare<{ slug: string }, string>("SELECT slug FROM bases WHERE slug = ?").get(slug);
}
