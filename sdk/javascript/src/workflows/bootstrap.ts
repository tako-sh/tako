/**
 * Worker-process bootstrap.
 *
 * Called from each runtime's worker entrypoint (`bun-worker.ts`,
 * `node-worker.ts`). Reads env vars set by tako-server,
 * attaches the RPC client, discovers `<appRoot>/workflows/`, and starts the worker
 * loop. The SDK never opens SQLite — tako-server owns the queue DB and
 * serves all state via the per-app enqueue socket.
 *
 * Env vars (set by tako-server when it spawns the worker):
 *   TAKO_INTERNAL_SOCKET       — path to the shared Tako internal unix socket
 *   TAKO_APP_NAME              — app name the worker belongs to
 *   TAKO_APP_ROOT              — JS app root for workflows discovery
 *   TAKO_WORKFLOW_WORKER       — optional worker group to load
 *   TAKO_WORKER_CONCURRENCY    — max parallel tasks per worker (default 500)
 *   TAKO_WORKER_IDLE_TIMEOUT_MS — scale-to-zero idle timeout; 0 = never
 *
 * Process IDs include a random component so restarts and separate hosts do
 * not share concurrency accounting. Each claim receives its own owner ID.
 */

import { randomUUID } from "node:crypto";
import { join } from "node:path";
import {
  assertInternalSocketEnvConsistency,
  installChannelSocketPublisherFromEnv,
} from "../tako/socket";
import { setWorkflowRuntime } from "./define";
import { DEFAULT_WORKER_GROUP, discoverWorkflows, workflowWorkerGroup } from "./discovery";
import { workflowsEngine } from "./engine";
import { WorkflowsClient } from "./rpc-client";
import { resolveAppRootDir } from "../tako/app-root";

export interface WorkerBootstrapOptions {
  /** Project directory. Defaults to `process.cwd()`. */
  appDir?: string;
  /** JavaScript app source root relative to `appDir`. Defaults to `TAKO_APP_ROOT` or `src`. */
  appRoot?: string;
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
  const workflowWorker = process.env[WORKFLOW_WORKER_ENV]?.trim() || DEFAULT_WORKER_GROUP;
  const workerId = `worker-${workflowWorker}-${process.pid}-${randomUUID()}`;

  workflowsEngine.configure({ client, workerId });
  setWorkflowRuntime({
    enqueue: (name, payload, opts) => workflowsEngine.enqueue(name, payload, opts),
    result: (name, id) => workflowsEngine.result(name, id),
    signal: (event, payload) => workflowsEngine.signal(event, payload),
  });

  const workflowsDir = join(resolveAppRootDir(appDir, opts.appRoot), WORKFLOWS_DIRNAME);
  const definitions = await discoverWorkflows(workflowsDir);
  for (const { name, handler, opts } of definitions) {
    if (workflowWorkerGroup(opts) === workflowWorker) {
      workflowsEngine.register(name, handler, opts);
    }
  }
  // Registration replaces the app-wide set. Claims remain scoped to this group.
  await client.registerSchedules(workflowsEngine.collectSchedules(definitions));

  const count = workflowsEngine.registeredNames.length;
  if (count === 0) {
    return { started: false, reason: "no workflows discovered", workflowCount: 0 };
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
