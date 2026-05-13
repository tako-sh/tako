import { Database } from "bun:sqlite";
import path from "node:path";
import { tako } from "tako.sh";

import type {
  BaseSnapshot,
  DbBase,
  DbSupplyRequest,
  MissionLogEvent,
  Step,
  StepState,
} from "./types";
import { EMPTY_RETRIES, EMPTY_STEPS } from "./types";

const l = tako.logger.child("db");
const db = openDb();
export const RECORD_RETENTION_MS = 3 * 24 * 60 * 60 * 1000;

function resolveDbPath(): string {
  return path.join(tako.dataDir, "mission.sqlite3");
}

function openDb(): Database {
  const db = new Database(resolveDbPath(), { create: true });
  db.run("PRAGMA journal_mode = WAL");
  db.run("PRAGMA synchronous = NORMAL");
  db.run("PRAGMA foreign_keys = ON");
  db.run(`
    CREATE TABLE IF NOT EXISTS bases (
      slug TEXT PRIMARY KEY,
      created_at INTEGER NOT NULL
    );
  `);
  db.run(`
    CREATE TABLE IF NOT EXISTS supply_requests (
      request_id TEXT PRIMARY KEY,
      base_slug TEXT NOT NULL,
      item TEXT NOT NULL,
      is_complete INTEGER NOT NULL,
      steps_json TEXT NOT NULL,
      retries_json TEXT NOT NULL,
      created_at INTEGER NOT NULL,
      updated_at INTEGER NOT NULL
    );
  `);
  db.run(
    "CREATE INDEX IF NOT EXISTS idx_requests_base ON supply_requests(base_slug, created_at DESC)",
  );
  return db;
}

type BaseRow = {
  slug: string;
  created_at: number;
};

type SupplyRequestRow = {
  request_id: string;
  base_slug: string;
  item: string;
  is_complete: number;
  steps_json: string;
  retries_json: string;
  created_at: number;
  updated_at: number;
};

type RequestStateRow = {
  is_complete: number;
  steps_json: string;
  retries_json: string;
};

export function ensureBase(slug: string): DbBase {
  const now = Date.now();
  db.prepare("INSERT OR IGNORE INTO bases (slug, created_at) VALUES (?, ?)").run(slug, now);
  const row = db
    .prepare<BaseRow, string>("SELECT slug, created_at FROM bases WHERE slug = ?")
    .get(slug);
  if (!row) {
    throw new Error(`Failed to upsert base '${slug}'`);
  }
  return { slug: row.slug, createdAt: row.created_at };
}

export function createRequest(input: {
  requestId: string;
  baseSlug: string;
  item: string;
}): DbSupplyRequest {
  const now = Date.now();
  const steps = { ...EMPTY_STEPS };
  const retries = { ...EMPTY_RETRIES };
  db.prepare(
    `INSERT INTO supply_requests
       (request_id, base_slug, item, is_complete, steps_json, retries_json, created_at, updated_at)
     VALUES (?, ?, ?, 0, ?, ?, ?, ?)`,
  ).run(
    input.requestId,
    input.baseSlug,
    input.item,
    JSON.stringify(steps),
    JSON.stringify(retries),
    now,
    now,
  );
  return {
    requestId: input.requestId,
    baseSlug: input.baseSlug,
    item: input.item,
    isComplete: false,
    steps,
    retries,
    createdAt: now,
    updatedAt: now,
  };
}

export function applyMissionEventToRequest(event: MissionLogEvent): DbSupplyRequest | null {
  const req = db
    .prepare<RequestStateRow, string>(
      "SELECT is_complete, steps_json, retries_json FROM supply_requests WHERE request_id = ?",
    )
    .get(event.requestId);

  if (!req) {
    l.warn("dropping orphan event for unknown request", {
      requestId: event.requestId,
      step: event.step ?? null,
      status: event.status ?? null,
    });
    return null;
  }

  const next = applyEvent(
    req.is_complete !== 0,
    JSON.parse(req.steps_json) as Record<Step, StepState>,
    JSON.parse(req.retries_json) as Record<Step, number>,
    event,
  );

  db.prepare(
    `UPDATE supply_requests
       SET is_complete = ?, steps_json = ?, retries_json = ?, updated_at = ?
     WHERE request_id = ?`,
  ).run(
    next.isComplete ? 1 : 0,
    JSON.stringify(next.steps),
    JSON.stringify(next.retries),
    event.timestamp,
    event.requestId,
  );

  const updated = db
    .prepare<SupplyRequestRow, string>(
      `SELECT request_id, base_slug, item, is_complete, steps_json, retries_json, created_at, updated_at
         FROM supply_requests
         WHERE request_id = ?`,
    )
    .get(event.requestId);

  return updated ? mapRequestRow(updated) : null;
}

function applyEvent(
  isComplete: boolean,
  steps: Record<Step, StepState>,
  retries: Record<Step, number>,
  event: MissionLogEvent,
): {
  isComplete: boolean;
  steps: Record<Step, StepState>;
  retries: Record<Step, number>;
} {
  if (!event.step || !event.status) return { isComplete, steps, retries };

  if (event.step === "complete") {
    return { isComplete: true, steps, retries };
  }

  if (event.step in steps) {
    const stepKey = event.step as Step;
    if (event.status === "failed") {
      return {
        isComplete,
        steps: { ...steps, [stepKey]: "failed" },
        retries: { ...retries, [stepKey]: retries[stepKey] + 1 },
      };
    }
    return { isComplete, steps: { ...steps, [stepKey]: event.status }, retries };
  }

  return { isComplete, steps, retries };
}

export function getBaseSnapshot(slug: string): BaseSnapshot {
  const base = ensureBase(slug);
  const requests = db
    .prepare<SupplyRequestRow, string>(
      `SELECT request_id, base_slug, item, is_complete, steps_json, retries_json, created_at, updated_at
         FROM supply_requests
         WHERE base_slug = ?
         ORDER BY created_at DESC
         LIMIT 50`,
    )
    .all(slug);
  return {
    base,
    requests: requests.map(mapRequestRow),
  };
}

export function cleanupOldRecords(now = Date.now()): {
  cutoff: number;
  requestsDeleted: number;
  basesDeleted: number;
} {
  const cutoff = now - RECORD_RETENTION_MS;
  const requestsDeleted = db
    .prepare("DELETE FROM supply_requests WHERE created_at < ?")
    .run(cutoff).changes;
  const basesDeleted = db
    .prepare(
      `DELETE FROM bases
         WHERE created_at < ?
           AND NOT EXISTS (
             SELECT 1 FROM supply_requests WHERE supply_requests.base_slug = bases.slug
           )`,
    )
    .run(cutoff).changes;

  return { cutoff, requestsDeleted, basesDeleted };
}

function mapRequestRow(row: SupplyRequestRow): DbSupplyRequest {
  return {
    requestId: row.request_id,
    baseSlug: row.base_slug,
    item: row.item,
    isComplete: row.is_complete !== 0,
    steps: JSON.parse(row.steps_json) as Record<Step, StepState>,
    retries: JSON.parse(row.retries_json) as Record<Step, number>,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}
