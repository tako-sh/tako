import { createClient, type Client } from "@libsql/client";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import type { MissionLogEvent } from "../src/server/types";

type DbModule = typeof import("../src/server/db");

const DAY_MS = 24 * 60 * 60 * 1000;

let dir = "";
let sql: Client;
let db: DbModule;

beforeAll(async () => {
  dir = await mkdtemp(path.join(tmpdir(), "tako-demo-db-"));
  sql = createClient({ url: `file:${path.join(dir, "mission.sqlite")}` });
  // Tako applies these before the app starts; tests apply them themselves.
  await sql.executeMultiple(
    await Bun.file(path.join(import.meta.dirname, "..", "migrations", "0001_initial.sql")).text(),
  );
  db = await import("../src/server/db");
  await db.useClient(sql);
});

afterAll(async () => {
  sql.close();
  await rm(dir, { recursive: true, force: true });
});

describe("supply requests", () => {
  test("events update a request's steps, retries, and completion", async () => {
    const requestId = crypto.randomUUID();
    await db.ensureBase("events-base");
    await db.createRequest({ requestId, baseSlug: "events-base", item: "ice cores" });

    const event = (step: MissionLogEvent["step"], status: MissionLogEvent["status"]) =>
      db.applyMissionEventToRequest({
        id: `${requestId}:${step}:${status}`,
        requestId,
        timestamp: Date.now(),
        source: "events-base",
        level: "info",
        message: "",
        step,
        status,
      });

    expect((await event("ship", "failed"))?.retries.ship).toBe(1);
    const shipped = await event("ship", "done");
    expect(shipped?.steps.ship).toBe("done");
    expect(shipped?.retries.ship).toBe(1);
    expect((await event("complete", "done"))?.isComplete).toBe(true);

    const snapshot = await db.getBaseSnapshot("events-base");
    expect(snapshot.requests.map((request) => request.requestId)).toEqual([requestId]);
    expect(snapshot.requests[0]?.isComplete).toBe(true);
  });

  test("events for unknown requests are dropped", async () => {
    const result = await db.applyMissionEventToRequest({
      id: "orphan",
      requestId: crypto.randomUUID(),
      timestamp: Date.now(),
      source: "nowhere",
      level: "info",
      message: "",
      step: "check",
      status: "done",
    });
    expect(result).toBeNull();
  });
});

describe("cleanupOldRecords", () => {
  test("cleanup workflow is scheduled daily", async () => {
    const workflow = await import("../src/workflows/cleanup");

    expect(workflow.default.definition.name).toBe("cleanup");
    expect(workflow.default.definition.opts.schedule).toBe("@daily");
  });

  test("deletes supply requests older than three days", async () => {
    const now = Date.now();
    const oldRequestId = crypto.randomUUID();
    const recentRequestId = crypto.randomUUID();

    await db.ensureBase("cleanup-requests");
    await db.createRequest({
      requestId: oldRequestId,
      baseSlug: "cleanup-requests",
      item: "old batteries",
    });
    await db.createRequest({
      requestId: recentRequestId,
      baseSlug: "cleanup-requests",
      item: "fresh oxygen",
    });

    await setRequestCreatedAt(oldRequestId, now - 4 * DAY_MS);
    await setRequestCreatedAt(recentRequestId, now - DAY_MS);

    expect(await db.cleanupOldRecords(now)).toEqual({
      cutoff: now - db.RECORD_RETENTION_MS,
      requestsDeleted: 1,
      basesDeleted: 0,
    });
    expect(await findRequest(oldRequestId)).toBeNull();
    expect(await findRequest(recentRequestId)).toBe(recentRequestId);
  });

  test("deletes stale empty bases but keeps bases with retained requests", async () => {
    const now = Date.now();
    const retainedRequestId = crypto.randomUUID();

    await db.ensureBase("empty-old-base");
    await db.ensureBase("active-old-base");
    await db.createRequest({
      requestId: retainedRequestId,
      baseSlug: "active-old-base",
      item: "reactor sealant",
    });

    await setBaseCreatedAt("empty-old-base", now - 4 * DAY_MS);
    await setBaseCreatedAt("active-old-base", now - 4 * DAY_MS);
    await setRequestCreatedAt(retainedRequestId, now - DAY_MS);

    expect(await db.cleanupOldRecords(now)).toEqual({
      cutoff: now - db.RECORD_RETENTION_MS,
      requestsDeleted: 0,
      basesDeleted: 1,
    });
    expect(await findBase("empty-old-base")).toBeNull();
    expect(await findBase("active-old-base")).toBe("active-old-base");
  });
});

async function setRequestCreatedAt(requestId: string, createdAt: number) {
  await sql.execute({
    sql: "UPDATE supply_requests SET created_at = ?, updated_at = ? WHERE request_id = ?",
    args: [createdAt, createdAt, requestId],
  });
}

async function setBaseCreatedAt(slug: string, createdAt: number) {
  await sql.execute({
    sql: "UPDATE bases SET created_at = ? WHERE slug = ?",
    args: [createdAt, slug],
  });
}

async function findRequest(requestId: string): Promise<string | null> {
  const result = await sql.execute({
    sql: "SELECT request_id FROM supply_requests WHERE request_id = ?",
    args: [requestId],
  });
  return (result.rows[0]?.["request_id"] as string | undefined) ?? null;
}

async function findBase(slug: string): Promise<string | null> {
  const result = await sql.execute({
    sql: "SELECT slug FROM bases WHERE slug = ?",
    args: [slug],
  });
  return (result.rows[0]?.["slug"] as string | undefined) ?? null;
}
