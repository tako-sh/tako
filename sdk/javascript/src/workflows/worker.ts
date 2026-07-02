/**
 * Worker loop — claims one run at a time, executes its handler with a
 * checkpointed step API, heartbeats the lease, and finalizes the run via
 * `complete` / `cancel` / `fail` / `defer` / `wait_for_event` based on the
 * outcome.
 *
 * Sentinel exceptions (BailSignal/FailSignal/DeferSignal/WaitSignal) drive
 * the non-retry termination paths cleanly.
 *
 * Start one Worker per app instance. `drain()` stops claiming and awaits
 * any in-flight run for the platform drain hook.
 */

import { createLogger, type Logger } from "../logger";
import { expBackoffMs } from "./backoff";
import type { WorkflowsClient } from "./rpc-client";
import {
  BailSignal,
  createStepAPI,
  DeferSignal,
  FailSignal,
  WaitSignal,
  type StepRunOptions,
  type StepWaitOptions,
  type WorkflowStepContext,
} from "./step";
import type { Run, StepState } from "./types";

/**
 * Function that executes one workflow run.
 *
 * The payload type `P` is inferred from `defineWorkflow<P>(...)` and flows into
 * `.enqueue(payload)`.
 */
export type WorkflowHandler<P = unknown> = (
  payload: P,
  ctx: WorkflowContext,
) => Promise<void> | void;

/**
 * Runtime context passed to each workflow handler.
 */
export interface WorkflowContext {
  /** Unique id for the current workflow run. */
  readonly runId: string;
  /** Name used when the workflow was registered with `defineWorkflow`. */
  readonly workflowName: string;
  /** Current run attempt, starting at 1. */
  readonly attempt: number;
  /** Logger scoped to this workflow run. */
  readonly logger: Logger;
  /**
   * Execute and memoize a durable step.
   *
   * On retry, completed steps return their stored result without calling `fn`.
   */
  run<T>(
    name: string,
    fn: (step: WorkflowStepContext) => Promise<T> | T,
    opts?: StepRunOptions,
  ): Promise<T>;
  /**
   * Durably sleep for `durationMs`.
   */
  sleep(name: string, durationMs: number): Promise<void>;
  /**
   * Park the run until `signal(name, payload)` wakes it or the optional timeout
   * elapses.
   */
  waitFor<T = unknown>(name: string, opts?: StepWaitOptions): Promise<T | null>;
  /**
   * End the run cleanly as `cancelled` with no retries.
   *
   * Use this for work that is no longer needed. To exit successfully early,
   * just `return` from the handler.
   */
  bail(reason?: string): never;
  /**
   * End the run as `dead` immediately with no retries.
   *
   * Use this for permanent errors that will not improve with retry.
   */
  fail(error: Error | string): never;
}

interface WorkflowRetryConfig {
  /** Run-level retry budget (default 3). */
  maxAttempts?: number;
  /** Run-level backoff between failed attempts. */
  backoff?: {
    /**
     * Initial backoff delay in ms.
     * @defaultValue 1_000
     */
    base?: number;
    /**
     * Maximum backoff delay in ms.
     * @defaultValue 3_600_000
     */
    max?: number;
  };
}

/** Options for constructing a workflow worker. */
export interface WorkerOptions {
  /** RPC client used for queue operations. */
  client: WorkflowsClient;
  /** Registered workflow handlers by workflow name. */
  registry: Map<string, RegisteredWorkflow>;
  /** Stable worker id used for leases. */
  workerId: string;
  /** @defaultValue 60_000 */
  leaseMs?: number;
  /** @defaultValue leaseMs / 3 */
  heartbeatIntervalMs?: number;
  /** @defaultValue 1_000 */
  pollIntervalMs?: number;
  /** @defaultValue 1_000 */
  baseBackoffMs?: number;
  /** @defaultValue 3_600_000 */
  maxBackoffMs?: number;
  /**
   * Scale-to-zero: exit poll loop after this many ms with no claim.
   * @defaultValue 0
   */
  idleTimeoutMs?: number;
  /**
   * Max concurrent in-flight runs.
   * @defaultValue 500
   */
  concurrency?: number;
  /** Base logger. Per-run children are tagged `worker:<workflowName>`. */
  logger?: Logger;
}

/** Workflow handler registered with the worker runtime. */
export interface RegisteredWorkflow {
  /** Function that executes one workflow run. */
  handler: WorkflowHandler;
  /** Optional run-level retry configuration. */
  retry?: WorkflowRetryConfig;
}

const DEFAULTS = {
  leaseMs: 60_000,
  pollIntervalMs: 1_000,
  baseBackoffMs: 1_000,
  maxBackoffMs: 3_600_000,
  idleTimeoutMs: 0,
  concurrency: 500,
} as const;

/**
 * Workflow worker loop.
 *
 * Claims runs from the workflow RPC client, executes handlers with durable
 * step APIs, heartbeats leases, and finalizes run state.
 */
export class Worker {
  private readonly client: WorkflowsClient;
  private readonly registry: Map<string, RegisteredWorkflow>;
  private readonly workerId: string;
  private readonly leaseMs: number;
  private readonly heartbeatIntervalMs: number;
  private readonly pollIntervalMs: number;
  private readonly baseBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly idleTimeoutMs: number;
  private readonly concurrency: number;
  private readonly log: Logger;

  private draining = false;
  private idledOut = false;
  private lastClaimAt = 0;
  private readonly inFlight = new Set<Promise<void>>();
  private loopPromise: Promise<void> | null = null;

  constructor(opts: WorkerOptions) {
    this.client = opts.client;
    this.registry = opts.registry;
    this.workerId = opts.workerId;
    this.leaseMs = opts.leaseMs ?? DEFAULTS.leaseMs;
    this.heartbeatIntervalMs = opts.heartbeatIntervalMs ?? Math.floor(this.leaseMs / 3);
    this.pollIntervalMs = opts.pollIntervalMs ?? DEFAULTS.pollIntervalMs;
    this.baseBackoffMs = opts.baseBackoffMs ?? DEFAULTS.baseBackoffMs;
    this.maxBackoffMs = opts.maxBackoffMs ?? DEFAULTS.maxBackoffMs;
    this.idleTimeoutMs = opts.idleTimeoutMs ?? DEFAULTS.idleTimeoutMs;
    this.concurrency = Math.max(1, opts.concurrency ?? DEFAULTS.concurrency);
    this.log = opts.logger ?? createLogger("worker");
    this.lastClaimAt = Date.now();
  }

  /** True when this worker exited because `idleTimeoutMs` elapsed. */
  get idled(): boolean {
    return this.idledOut;
  }

  /** Claim and process one run if available. */
  async processOnce(): Promise<boolean> {
    if (this.draining) return false;
    const names = Array.from(this.registry.keys());
    const run = await this.client.claim(this.workerId, names, this.leaseMs);
    if (!run) return false;
    this.lastClaimAt = Date.now();

    const work = this.execute(run);
    this.inFlight.add(work);
    try {
      await work;
    } finally {
      this.inFlight.delete(work);
    }
    return true;
  }

  /**
   * Claim one run and kick off its execution without awaiting. Returns
   * `true` if a run was claimed and dispatched, `false` if the queue was
   * empty. Used by the internal run loop to keep up to `concurrency`
   * runs in flight simultaneously; tests use the awaiting `processOnce`.
   */
  private async dispatchOne(): Promise<boolean> {
    if (this.draining) return false;
    const names = Array.from(this.registry.keys());
    const run = await this.client.claim(this.workerId, names, this.leaseMs);
    if (!run) return false;
    this.lastClaimAt = Date.now();

    const work: Promise<void> = this.execute(run)
      .catch((err) => {
        // Nothing awaits this promise; without a catch, a failed finalize
        // RPC (complete/fail/cancel) becomes an unhandled rejection.
        this.log.error("run finalization failed", {
          runId: run.id,
          workflow: run.name,
          error: err instanceof Error ? err.message : String(err),
        });
      })
      .finally(() => {
        this.inFlight.delete(work);
      });
    this.inFlight.add(work);
    return true;
  }

  /** Start the polling loop. */
  start(): void {
    if (this.loopPromise) return;
    this.loopPromise = this.runLoop();
  }

  /** Stop claiming new runs and wait for active runs to finish. */
  async drain(): Promise<void> {
    this.draining = true;
    if (this.loopPromise) {
      await this.loopPromise;
      this.loopPromise = null;
    }
    await Promise.allSettled(Array.from(this.inFlight));
  }

  /** Number of currently running workflow handlers. */
  get runningCount(): number {
    return this.inFlight.size;
  }

  private async runLoop(): Promise<void> {
    while (!this.draining) {
      if (this.inFlight.size >= this.concurrency) {
        await Promise.race(Array.from(this.inFlight)).catch(() => {});
        continue;
      }
      const did = await this.dispatchOne().catch(() => false);
      if (!did && !this.draining) {
        if (
          this.idleTimeoutMs > 0 &&
          this.inFlight.size === 0 &&
          Date.now() - this.lastClaimAt >= this.idleTimeoutMs
        ) {
          this.idledOut = true;
          this.draining = true;
          break;
        }
        await new Promise((r) => setTimeout(r, this.pollIntervalMs));
      }
    }
  }

  private async execute(run: Run): Promise<void> {
    const runLog = this.log.child(`worker:${run.name}`, { runId: run.id });
    const contextLog = this.log.child(run.name, { runId: run.id, workflow: run.name });
    const reg = this.registry.get(run.name);
    if (!reg) {
      runLog.error("Workflow failed", { error: `no handler registered for '${run.name}'` });
      await this.client.fail(
        run.id,
        this.workerId,
        `no handler registered for '${run.name}'`,
        null,
        true,
      );
      return;
    }

    const stepState: StepState = { ...run.stepState };
    const createStepContext = (stepName: string): WorkflowStepContext => ({
      runId: run.id,
      workflowName: run.name,
      stepName,
      attempt: run.attempts,
      logger: contextLog.child(`${run.name}:${stepName}`, { step: stepName }),
    });
    const context: WorkflowContext = {
      runId: run.id,
      workflowName: run.name,
      attempt: run.attempts,
      logger: contextLog,
      ...createStepAPI(this.client, run.id, this.workerId, stepState, runLog, createStepContext),
      bail: (reason?: string): never => {
        throw new BailSignal(reason);
      },
      fail: (error: Error | string): never => {
        const e = error instanceof Error ? error : new Error(error);
        throw new FailSignal(e);
      },
    };

    let heartbeatTimer: ReturnType<typeof setInterval> | null = null;
    if (this.heartbeatIntervalMs > 0) {
      heartbeatTimer = setInterval(() => {
        this.client.heartbeat(run.id, this.workerId, this.leaseMs).catch(() => {});
      }, this.heartbeatIntervalMs);
    }

    runLog.info("Workflow started", { attempt: run.attempts, payload: run.payload });
    try {
      await reg.handler(run.payload, context);
      await this.client.complete(run.id, this.workerId);
      runLog.info("Workflow completed");
    } catch (err) {
      if (err instanceof BailSignal) {
        await this.client.cancel(run.id, this.workerId, err.reason ?? null);
        runLog.info("Workflow cancelled", { reason: err.reason ?? null });
        return;
      }
      if (err instanceof FailSignal) {
        await this.client.fail(run.id, this.workerId, err.error.message, null, true);
        runLog.error("Workflow failed", { error: err.error });
        return;
      }
      if (err instanceof DeferSignal) {
        await this.client.defer(run.id, this.workerId, err.wakeAt);
        runLog.debug("Workflow deferred", { wakeAt: err.wakeAt });
        return;
      }
      if (err instanceof WaitSignal) {
        await this.client.waitForEvent(
          run.id,
          this.workerId,
          err.stepName,
          err.eventName,
          err.timeoutAt,
        );
        runLog.debug("Workflow waiting", { event: err.eventName, timeoutAt: err.timeoutAt });
        return;
      }

      // Regular error → run-level retry path.
      const message = err instanceof Error ? err.message : String(err);
      const maxAttempts = reg.retry?.maxAttempts ?? run.retries + 1;
      const finalize = run.attempts >= maxAttempts;
      const base = reg.retry?.backoff?.base ?? this.baseBackoffMs;
      const max = reg.retry?.backoff?.max ?? this.maxBackoffMs;
      const nextRunAt = finalize
        ? null
        : new Date(Date.now() + expBackoffMs(run.attempts, base, max));
      await this.client.fail(run.id, this.workerId, message, nextRunAt, finalize);
      const errField = err instanceof Error ? err : message;
      if (finalize) {
        runLog.error("Workflow failed", { attempt: run.attempts, error: errField });
      } else {
        runLog.warn("Workflow failed, retrying", {
          attempt: run.attempts,
          nextRunAt,
          error: errField,
        });
      }
    } finally {
      if (heartbeatTimer) clearInterval(heartbeatTimer);
    }
  }
}
