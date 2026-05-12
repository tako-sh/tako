/**
 * Shared types for Tako's durable workflow engine.
 *
 * Vocabulary:
 *   workflow — a named handler (the thing you write in `<app_root>/workflows/*.ts`)
 *   run      — one execution of a workflow (the row in the queue)
 *   step     — a memoized portion inside a run (via `ctx.run`)
 */

import type { WorkflowHandler } from "./worker";

/** Unique id for a workflow run. */
export type RunId = string;

/** Lifecycle state for a workflow run. */
export type RunStatus = "pending" | "running" | "succeeded" | "cancelled" | "dead";

/** Persisted memoized step results for a workflow run. */
export type StepState = Record<string, unknown>;

/** Wire shape used to enqueue a workflow run. */
export interface RunSpec {
  /**
   * Workflow name — the filename stem of the handler file.
   * @example "send-welcome" // <app_root>/workflows/send-welcome.ts
   */
  name: string;
  /** JSON-serializable user payload. */
  payload: unknown;
  /**
   * When to run.
   * @defaultValue now
   */
  runAt?: Date;
  /** Number of retries after the first attempt. */
  retries?: number;
  /**
   * Uniqueness key. If a run with this key already exists in a
   * non-terminal state, enqueue is a no-op and the existing run id is
   * returned. Used by cron to avoid duplicate ticks across replicas.
   */
  uniqueKey?: string | null;
}

/** Server-owned record for one workflow execution. */
export interface Run {
  /** Unique run id. */
  id: RunId;
  /** Workflow name. */
  name: string;
  /** JSON-serializable payload passed to the workflow handler. */
  payload: unknown;
  /** Current run status. */
  status: RunStatus;
  /** Number of attempts already made. */
  attempts: number;
  /** Retry count after the first attempt. */
  retries: number;
  /** Unix ms. */
  runAt: number;
  /** Unix ms; null for non-running runs. */
  leaseUntil: number | null;
  /** Worker id currently holding the lease, or null when not running. */
  workerId: string | null;
  /** Last error message for failed attempts. */
  lastError: string | null;
  /** Persisted durable step results. */
  stepState: StepState;
  /** Unix ms. */
  createdAt: number;
  /** Enqueue deduplication key, when provided. */
  uniqueKey: string | null;
}

/** Workflow definition options passed to `defineWorkflow`. */
export interface WorkflowOpts<P = unknown> {
  /** Workflow body. The payload type flows into `.enqueue(payload)`. */
  handler: WorkflowHandler<P>;
  /**
   * Worker group that should execute this workflow when a worker process is
   * launched with a matching `TAKO_WORKFLOW_WORKER` value.
   *
   * Omit for the default worker group.
   * @defaultValue "default"
   */
  worker?: string;
  /**
   * Number of retries after the first attempt.
   * @defaultValue 2
   */
  retries?: number;
  /** Run-level backoff between failed attempts. `base` defaults to 1 000 ms; `max` to 3 600 000 ms. */
  backoff?: { base?: number; max?: number };
  /**
   * Worker concurrency per instance.
   * @defaultValue 10
   */
  concurrency?: number;
  /**
   * Handler timeout in ms.
   * @defaultValue Infinity
   */
  timeoutMs?: number;
  /**
   * Cron expression (5-field: minute hour day-of-month month day-of-week).
   * @example "0 9 * * 1-5"    — weekdays at 9am
   * @example "&#42;/15 * * * *" — every 15 minutes
   */
  schedule?: string;
}

export type WorkflowRuntimeOpts = Omit<WorkflowOpts, "handler">;
