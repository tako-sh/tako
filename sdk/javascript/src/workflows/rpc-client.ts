/**
 * WorkflowsClient — single client for all workflow RPCs.
 *
 * Runs in the HTTP app process (for workflow `.enqueue()` and
 * `workflowsEngine.signal()`) and in the worker process (for claim, heartbeat,
 * saveStep, complete, cancel, fail, defer, waitForEvent). The SDK never
 * touches SQLite — tako-server owns the queue file; everything reaches it
 * via the shared internal unix socket at `TAKO_INTERNAL_SOCKET`.
 *
 * Every command carries the app name (from `TAKO_APP_NAME`), so one
 * tako-server socket can route for every deployed app.
 */

import { APP_NAME_ENV, INTERNAL_SOCKET_ENV, TakoError, callInternal } from "../tako/socket";
import type { EnqueueOptions } from "./engine";
import type { Run, RunId, RunStatus, StepState } from "./types";

/** Result returned after enqueueing a workflow run. */
export interface EnqueueResult {
  /** Workflow run id. */
  id: RunId;
  /** True when `uniqueKey` matched an existing non-terminal run. */
  deduplicated: boolean;
}

/**
 * RPC client for workflow operations against the Tako internal socket.
 *
 * Intended for Tako runtime internals; app code should use workflow handles
 * returned by {@link import("./define").defineWorkflow}.
 */
export class WorkflowsClient {
  private readonly socketPath: string;
  private readonly app: string;

  constructor(socketPath: string, app: string) {
    this.socketPath = socketPath;
    this.app = app;
  }

  /**
   * Build a client from env vars set by tako-server when spawning the
   * app/worker process. Returns null when the env vars are absent (outside
   * of a Tako-managed process) — callers should fall back or error.
   */
  static fromEnv(): WorkflowsClient | null {
    const path = process.env[INTERNAL_SOCKET_ENV];
    const app = process.env[APP_NAME_ENV];
    if (!path || !app) return null;
    return new WorkflowsClient(path, app);
  }

  /** The app name this client sends on every RPC. */
  get appName(): string {
    return this.app;
  }

  // --- Enqueue / signal: usable from any process ---

  /**
   * Enqueue a workflow run.
   *
   * @param name - Workflow name.
   * @param payload - JSON-serializable workflow payload.
   * @param opts - Enqueue options.
   * @defaultValue opts = {}
   */
  async enqueue(name: string, payload: unknown, opts: EnqueueOptions = {}): Promise<EnqueueResult> {
    const wire: Record<string, unknown> = {};
    if (opts.runAt !== undefined) wire["run_at_ms"] = opts.runAt.getTime();
    if (opts.retries !== undefined) wire["max_attempts"] = opts.retries + 1;
    if (opts.uniqueKey !== undefined && opts.uniqueKey !== null) {
      wire["unique_key"] = opts.uniqueKey;
    }
    const data = await this.call({
      command: "enqueue_run",
      app: this.app,
      name,
      payload: payload ?? null,
      opts: wire,
    });
    const d = data as { id: string; deduplicated: boolean } | null;
    if (!d || typeof d.id !== "string") {
      throw new TakoError("TAKO_PROTOCOL", "Internal Server Error");
    }
    return { id: d.id, deduplicated: Boolean(d.deduplicated) };
  }

  /** Wake workflow runs parked on `ctx.waitFor(eventName)`. */
  async signal(eventName: string, payload: unknown): Promise<number> {
    const data = await this.call({
      command: "signal",
      app: this.app,
      event_name: eventName,
      payload: payload ?? null,
    });
    const d = data as { woken?: number } | null;
    return d?.woken ?? 0;
  }

  // --- Worker-only: registration + run lifecycle ---

  /** Register cron schedules for discovered workflows. */
  async registerSchedules(schedules: Array<{ name: string; cron: string }>): Promise<void> {
    await this.call({ command: "register_schedules", app: this.app, schedules });
  }

  /** Claim one pending workflow run for a worker. */
  async claim(workerId: string, names: string[], leaseMs: number): Promise<Run | null> {
    const data = await this.call({
      command: "claim_run",
      app: this.app,
      worker_id: workerId,
      names,
      lease_ms: leaseMs,
    });
    if (data === null || data === undefined) return null;
    return rawToRun(data as RawRun);
  }

  /** Extend the lease for a running workflow run. */
  async heartbeat(id: RunId, workerId: string, leaseMs: number): Promise<void> {
    await this.call({
      command: "heartbeat_run",
      app: this.app,
      id,
      worker_id: workerId,
      lease_ms: leaseMs,
    });
  }

  /** Persist the memoized result for one durable step. */
  async saveStep(id: RunId, workerId: string, stepName: string, result: unknown): Promise<void> {
    await this.call({
      command: "save_step",
      app: this.app,
      id,
      worker_id: workerId,
      step_name: stepName,
      result: result ?? null,
    });
  }

  /** Mark a workflow run as succeeded. */
  async complete(id: RunId, workerId: string): Promise<void> {
    await this.call({ command: "complete_run", app: this.app, id, worker_id: workerId });
  }

  /** Mark a workflow run as cancelled. */
  async cancel(id: RunId, workerId: string, reason?: string | null): Promise<void> {
    await this.call({
      command: "cancel_run",
      app: this.app,
      id,
      worker_id: workerId,
      reason: reason ?? null,
    });
  }

  /** Mark a workflow run attempt as failed or dead. */
  async fail(
    id: RunId,
    workerId: string,
    error: string,
    nextRunAt: Date | null,
    finalize: boolean,
  ): Promise<void> {
    await this.call({
      command: "fail_run",
      app: this.app,
      id,
      worker_id: workerId,
      error,
      next_run_at_ms: nextRunAt ? nextRunAt.getTime() : null,
      finalize,
    });
  }

  /** Defer a workflow run until a time, or indefinitely when `wakeAt` is null. */
  async defer(id: RunId, workerId: string, wakeAt: Date | null): Promise<void> {
    await this.call({
      command: "defer_run",
      app: this.app,
      id,
      worker_id: workerId,
      wake_at_ms: wakeAt ? wakeAt.getTime() : null,
    });
  }

  /** Park a workflow run until an event or optional timeout. */
  async waitForEvent(
    id: RunId,
    workerId: string,
    stepName: string,
    eventName: string,
    timeoutAt: Date | null,
  ): Promise<void> {
    await this.call({
      command: "wait_for_event",
      app: this.app,
      id,
      worker_id: workerId,
      step_name: stepName,
      event_name: eventName,
      timeout_at_ms: timeoutAt ? timeoutAt.getTime() : null,
    });
  }

  // --- Internal ---

  private call(cmd: unknown): Promise<unknown> {
    return callInternal(this.socketPath, cmd);
  }
}

interface RawRun {
  id: string;
  name: string;
  payload: unknown;
  status: string;
  attempts: number;
  max_attempts: number;
  run_at_ms: number;
  step_state: StepState;
}

function rawToRun(raw: RawRun): Run {
  return {
    id: raw.id,
    name: raw.name,
    payload: raw.payload,
    status: raw.status as RunStatus,
    attempts: raw.attempts,
    retries: raw.max_attempts - 1,
    runAt: raw.run_at_ms,
    leaseUntil: null,
    workerId: null,
    lastError: null,
    stepState: raw.step_state ?? {},
    createdAt: 0,
    uniqueKey: null,
  };
}
