/**
 * Worker-process bootstrap.
 *
 * Called from each runtime's worker entrypoint (`bun-worker.ts`,
 * `node-worker.ts`). Reads env vars set by tako-server,
 * attaches the RPC client, discovers `workflows/`, and starts the worker
 * loop. The SDK never opens SQLite — tako-server owns the queue DB and
 * serves all state via the per-app enqueue socket.
 *
 * Env vars (set by tako-server when it spawns the worker):
 *   TAKO_INTERNAL_SOCKET       — path to the shared Tako internal unix socket
 *   TAKO_APP_NAME              — app name the worker belongs to
 *   TAKO_WORKFLOW_WORKER       — optional worker group to load
 *   TAKO_WORKER_CONCURRENCY    — max parallel tasks per worker (default 10)
 *   TAKO_WORKER_IDLE_TIMEOUT_MS — scale-to-zero idle timeout; 0 = never
 *
 * The claim leaseholder id is always `worker-<pid>` — the PID is the
 * useful forensic when a run goes orphaned (matches the process that
 * died in host logs), and there's no platform-level need for a separate
 * identifier.
 */

import { join } from "node:path";
import {
  assertInternalSocketEnvConsistency,
  installChannelSocketPublisherFromEnv,
} from "../tako/socket";
import { setWorkflowRuntime } from "./define";
import { workflowsEngine } from "./engine";
import { WorkflowsClient } from "./rpc-client";

export interface WorkerBootstrapOptions {
  /** Directory containing the `workflows/` subdir. Defaults to `process.cwd()`. */
  appDir?: string;
}

export interface WorkerBootstrapResult {
  started: boolean;
  reason?: string;
  workflowCount: number;
}

const WORKFLOWS_DIRNAME = "workflows";
const WORKFLOW_WORKER_ENV = "TAKO_WORKFLOW_WORKER";

export async function bootstrapWorker(
  opts: WorkerBootstrapOptions = {},
): Promise<WorkerBootstrapResult> {
  assertInternalSocketEnvConsistency();

  const appDir = opts.appDir ?? process.cwd();
  const client = WorkflowsClient.fromEnv();
  if (!client) {
    return {
      started: false,
      reason: "TAKO_INTERNAL_SOCKET / TAKO_APP_NAME not set",
      workflowCount: 0,
    };
  }

  installChannelSocketPublisherFromEnv();

  const concurrency = parseIntEnv("TAKO_WORKER_CONCURRENCY", 500);
  const idleTimeoutMs = parseIntEnv("TAKO_WORKER_IDLE_TIMEOUT_MS", 0);
  const workflowWorker = process.env[WORKFLOW_WORKER_ENV]?.trim() || undefined;
  const workerId = workflowWorker
    ? `worker-${workflowWorker}-${process.pid}`
    : `worker-${process.pid}`;

  workflowsEngine.configure({ client, workerId });
  setWorkflowRuntime({
    enqueue: (name, payload, opts) => workflowsEngine.enqueue(name, payload, opts),
    signal: (event, payload) => workflowsEngine.signal(event, payload),
  });

  const workflowsDir = join(appDir, WORKFLOWS_DIRNAME);
  const count = await workflowsEngine.discover(
    workflowsDir,
    workflowWorker === undefined ? {} : { worker: workflowWorker },
  );
  if (count === 0) {
    return { started: false, reason: "no workflows discovered", workflowCount: 0 };
  }

  // Tell the server about any cron schedules.
  const schedules = workflowsEngine.collectSchedules();
  if (schedules.length > 0) {
    await client.registerSchedules(schedules);
  }

  workflowsEngine.startWorker({ concurrency, idleTimeoutMs });
  return { started: true, workflowCount: count };
}

function parseIntEnv(name: string, fallback: number): number {
  const raw = process.env[name];
  if (!raw) return fallback;
  const n = Number.parseInt(raw, 10);
  return Number.isFinite(n) && n >= 0 ? n : fallback;
}
