import { createClient, type Client, type Row } from "@libsql/client";
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
export const RECORD_RETENTION_MS = 3 * 24 * 60 * 60 * 1000;

const REQUEST_COLUMNS =
  "request_id, base_slug, item, is_complete, steps_json, retries_json, created_at, updated_at";

let connection: Promise<Client> | undefined;

/**
 * The `mission` database bound in tako.toml. Tako serves it next to the app,
 * from a local file under `tako dev` and from its server once deployed, and
 * applies `migrations/` to it before the app starts.
 */
function db(): Promise<Client> {
  connection ??= Promise.resolve(createClient(missionDatabase()));
  return connection;
}

function missionDatabase() {
  const database = tako.databases.mission;
  if (!database) {
    throw new Error(
      "No database is bound as `mission`. Create one with `tako sqlite create mission --env <env>`.",
    );
  }
  return database;
}

/** Uses `client` instead of the bound database. For tests. */
export function useClient(client: Client): Promise<Client> {
  connection = Promise.resolve(client);
  return connection;
}

export async function ensureBase(slug: string): Promise<DbBase> {
  const client = await db();
  const [, result] = await client.batch(
    [
      {
        sql: "INSERT OR IGNORE INTO bases (slug, created_at) VALUES (?, ?)",
        args: [slug, Date.now()],
      },
      { sql: "SELECT slug, created_at FROM bases WHERE slug = ?", args: [slug] },
    ],
    "write",
  );
  const row = result?.rows[0];
  if (!row) {
    throw new Error(`Failed to upsert base '${slug}'`);
  }
  return { slug: String(row["slug"]), createdAt: Number(row["created_at"]) };
}

export async function createRequest(input: {
  requestId: string;
  baseSlug: string;
  item: string;
}): Promise<DbSupplyRequest> {
  const now = Date.now();
  const steps = { ...EMPTY_STEPS };
  const retries = { ...EMPTY_RETRIES };
  const client = await db();
  await client.execute({
    sql: `INSERT INTO supply_requests (${REQUEST_COLUMNS})
          VALUES (?, ?, ?, 0, ?, ?, ?, ?)`,
    args: [
      input.requestId,
      input.baseSlug,
      input.item,
      JSON.stringify(steps),
      JSON.stringify(retries),
      now,
      now,
    ],
  });
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

export async function applyMissionEventToRequest(
  event: MissionLogEvent,
): Promise<DbSupplyRequest | null> {
  const client = await db();
  // Read, apply, and write in one transaction so concurrent events for a
  // request cannot overwrite each other's steps.
  const tx = await client.transaction("write");
  try {
    const current = await tx.execute({
      sql: "SELECT is_complete, steps_json, retries_json FROM supply_requests WHERE request_id = ?",
      args: [event.requestId],
    });
    const req = current.rows[0];
    if (!req) {
      await tx.rollback();
      l.warn("dropping orphan event for unknown request", {
        requestId: event.requestId,
        step: event.step ?? null,
        status: event.status ?? null,
      });
      return null;
    }

    const next = applyEvent(
      Number(req["is_complete"]) !== 0,
      JSON.parse(String(req["steps_json"])) as Record<Step, StepState>,
      JSON.parse(String(req["retries_json"])) as Record<Step, number>,
      event,
    );

    const [, updated] = await tx.batch([
      {
        sql: `UPDATE supply_requests
                SET is_complete = ?, steps_json = ?, retries_json = ?, updated_at = ?
              WHERE request_id = ?`,
        args: [
          next.isComplete ? 1 : 0,
          JSON.stringify(next.steps),
          JSON.stringify(next.retries),
          event.timestamp,
          event.requestId,
        ],
      },
      {
        sql: `SELECT ${REQUEST_COLUMNS} FROM supply_requests WHERE request_id = ?`,
        args: [event.requestId],
      },
    ]);
    await tx.commit();
    const row = updated?.rows[0];
    return row ? mapRequestRow(row) : null;
  } finally {
    tx.close();
  }
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

export async function getBaseSnapshot(slug: string): Promise<BaseSnapshot> {
  const base = await ensureBase(slug);
  const client = await db();
  const requests = await client.execute({
    sql: `SELECT ${REQUEST_COLUMNS}
            FROM supply_requests
            WHERE base_slug = ?
            ORDER BY created_at DESC
            LIMIT 50`,
    args: [slug],
  });
  return {
    base,
    requests: requests.rows.map(mapRequestRow),
  };
}

export async function cleanupOldRecords(now = Date.now()): Promise<{
  cutoff: number;
  requestsDeleted: number;
  basesDeleted: number;
}> {
  const cutoff = now - RECORD_RETENTION_MS;
  const client = await db();
  const [requests, bases] = await client.batch(
    [
      { sql: "DELETE FROM supply_requests WHERE created_at < ?", args: [cutoff] },
      {
        sql: `DELETE FROM bases
                WHERE created_at < ?
                  AND NOT EXISTS (
                    SELECT 1 FROM supply_requests WHERE supply_requests.base_slug = bases.slug
                  )`,
        args: [cutoff],
      },
    ],
    "write",
  );

  return {
    cutoff,
    requestsDeleted: requests?.rowsAffected ?? 0,
    basesDeleted: bases?.rowsAffected ?? 0,
  };
}

function mapRequestRow(row: Row): DbSupplyRequest {
  return {
    requestId: String(row["request_id"]),
    baseSlug: String(row["base_slug"]),
    item: String(row["item"]),
    isComplete: Number(row["is_complete"]) !== 0,
    steps: JSON.parse(String(row["steps_json"])) as Record<Step, StepState>,
    retries: JSON.parse(String(row["retries_json"])) as Record<Step, number>,
    createdAt: Number(row["created_at"]),
    updatedAt: Number(row["updated_at"]),
  };
}
