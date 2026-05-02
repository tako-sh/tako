/**
 * Shared helper for dev entrypoints (bun-dev / node-dev) that
 * boot the workflow worker in the same process as the HTTP server.
 *
 * Worker-side logs (anything printed during `bootstrapWorker()`) are
 * prefixed with `[worker]` to distinguish them from request-serving
 * output. The worker is scale-to-zero — it sits idle until the first
 * enqueue or cron tick and costs effectively nothing in the meantime.
 */

import { bootstrapWorker } from "../workflows/bootstrap";
import { workflowsEngine } from "../workflows/engine";

export async function startInProcessWorker(): Promise<void> {
  const prefix = "[worker]";
  const originalError = console.error.bind(console);
  const originalLog = console.log.bind(console);
  let inWorkerScope = false;

  const wrap =
    (orig: (...args: unknown[]) => void) =>
    (...args: unknown[]): void => {
      if (inWorkerScope) orig(prefix, ...args);
      else orig(...args);
    };
  console.error = wrap(originalError);
  console.log = wrap(originalLog);

  inWorkerScope = true;
  try {
    const result = await bootstrapWorker();
    if (!result.started) {
      originalError(`${prefix} not started: ${result.reason ?? "unknown"}`);
      return;
    }
    originalError(`${prefix} running ${result.workflowCount} workflow(s)`);
  } catch (err) {
    originalError(`${prefix} bootstrap failed:`, err);
  } finally {
    inWorkerScope = false;
  }
}

export function drainInProcessWorker(): Promise<void> {
  return workflowsEngine.drain();
}
