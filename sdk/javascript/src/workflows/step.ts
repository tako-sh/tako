/**
 * Step API + workflow control signals.
 *
 * `ctx.run(name, fn, opts?)` memoizes fn's result in the run's steps
 * table. On retry, completed steps return their stored value instead of
 * re-executing. Per-step `retries`/`backoff`/`retry: false` options control
 * in-step retry behavior independent of the run-level retry budget.
 *
 * `ctx.sleep(name, durationMs)` waits durably. Short sleeps are
 * inline; long sleeps (≥INLINE_SLEEP_THRESHOLD_MS) defer the run via
 * `client.defer` so the worker can release.
 *
 * `ctx.waitFor(name, opts?)` parks the run until a matching
 * `workflowsEngine.signal(eventName, payload)` (from `tako.sh/internal`) arrives or the timeout fires.
 * Resumption hydrates the event payload as the step's result.
 *
 * **At-least-once contract**: if the worker dies between fn() returning and
 * saveStep persisting, fn re-runs on next claim. Make step bodies
 * idempotent (Stripe idempotency keys, upsert not insert, etc.).
 */

import type { Logger } from "../logger";
import { expBackoffMs } from "./backoff";
import type { WorkflowsClient } from "./rpc-client";
import type { RunId, StepState } from "./types";

const INLINE_SLEEP_THRESHOLD_MS = 30_000;

/** Options for `ctx.run(name, fn, options)`. */
export interface StepRunOptions {
  /**
   * In-step retry attempts before propagating.
   * @defaultValue 0
   */
  retries?: number;
  /** Backoff between in-step retries. */
  backoff?: {
    /**
     * Initial backoff delay in ms.
     * @defaultValue 1_000
     */
    base?: number;
    /**
     * Maximum backoff delay in ms.
     * @defaultValue 30_000
     */
    max?: number;
  };
  /** When set to `false`, any throw inside fn fails the run immediately (skips in-step retries). */
  retry?: false;
}

/** Options for `ctx.waitFor(name, options)`. */
export interface StepWaitOptions {
  /**
   * Timeout in ms. After this elapses without a matching signal, the step
   * resolves to `null`.
   * @defaultValue Infinity (parked indefinitely)
   */
  timeout?: number;
}

interface StepAPI {
  run<T>(
    name: string,
    fn: (step: WorkflowStepContext) => Promise<T> | T,
    opts?: StepRunOptions,
  ): Promise<T>;
  sleep(name: string, durationMs: number): Promise<void>;
  waitFor<T = unknown>(name: string, opts?: StepWaitOptions): Promise<T | null>;
}

/** Context passed to one durable workflow step. */
export interface WorkflowStepContext {
  /** Unique id for the current workflow run. */
  readonly runId: RunId;
  /** Name of the current workflow. */
  readonly workflowName: string;
  /** Name of the current durable step. */
  readonly stepName: string;
  /** Current run attempt, starting at 1. */
  readonly attempt: number;
  /** Logger scoped to this step. */
  readonly logger: Logger;
}

/** Sentinel: end the run cleanly as `cancelled`. */
export class BailSignal {
  constructor(public reason?: string) {}
}

/** Sentinel: end the run as `dead` immediately (skip retries). */
export class FailSignal {
  constructor(public error: Error) {}
}

/** Sentinel: defer the run to `wakeAt` (or indefinitely if null). */
export class DeferSignal {
  constructor(public wakeAt: Date | null) {}
}

/** Sentinel: park the run waiting for an event. */
export class WaitSignal {
  constructor(
    public stepName: string,
    public eventName: string,
    public timeoutAt: Date | null,
  ) {}
}

/** True for any control-flow sentinel — these must propagate untouched. */
export function isControlSignal(err: unknown): boolean {
  return (
    err instanceof BailSignal ||
    err instanceof FailSignal ||
    err instanceof DeferSignal ||
    err instanceof WaitSignal
  );
}

/**
 * Create the durable step API for one claimed workflow run.
 *
 * @internal
 */
export function createStepAPI(
  client: WorkflowsClient,
  runId: RunId,
  workerId: string,
  stepState: StepState,
  log: Logger,
  createContext: (stepName: string) => WorkflowStepContext,
): StepAPI {
  return {
    async run<T>(
      name: string,
      fn: (step: WorkflowStepContext) => Promise<T> | T,
      opts?: StepRunOptions,
    ): Promise<T> {
      if (Object.prototype.hasOwnProperty.call(stepState, name)) {
        log.debug("Step cached", { step: name });
        return stepState[name] as T;
      }

      const attempts = (opts?.retries ?? 0) + 1;
      const base = opts?.backoff?.base ?? 1_000;
      const max = opts?.backoff?.max ?? 30_000;

      let lastErr: unknown;
      const startedAt = Date.now();
      const context = createContext(name);
      for (let attempt = 1; attempt <= attempts; attempt++) {
        try {
          const result = await fn(context);
          stepState[name] = result as unknown;
          await client.saveStep(runId, workerId, name, result ?? null);
          log.info("Step completed", { step: name, ms: Date.now() - startedAt });
          return result;
        } catch (err) {
          // Control signals (success/bail/fail/defer/wait) are how the
          // handler talks to the worker — never retry, never wrap, just
          // propagate.
          if (isControlSignal(err)) throw err;
          lastErr = err;
          if (opts?.retry === false) {
            const e = err instanceof Error ? err : new Error(String(err));
            throw new FailSignal(e);
          }
          if (attempt < attempts) {
            await new Promise((r) => setTimeout(r, expBackoffMs(attempt, base, max)));
          }
        }
      }
      throw lastErr;
    },

    async sleep(name: string, durationMs: number): Promise<void> {
      const key = `__sleep:${name}`;
      const stored = stepState[key] as { wakeAt: number } | undefined;
      if (stored) {
        if (Date.now() >= stored.wakeAt) {
          if (!Object.prototype.hasOwnProperty.call(stepState, name)) {
            stepState[name] = true;
            await client.saveStep(runId, workerId, name, true);
            log.info("Sleep completed", { step: name, ms: durationMs });
          } else {
            log.debug("Sleep cached", { step: name });
          }
          return;
        }
        throw new DeferSignal(new Date(stored.wakeAt));
      }

      const wakeAt = Date.now() + durationMs;
      stepState[key] = { wakeAt };
      await client.saveStep(runId, workerId, key, { wakeAt });

      if (durationMs < INLINE_SLEEP_THRESHOLD_MS) {
        await new Promise((r) => setTimeout(r, durationMs));
        stepState[name] = true;
        await client.saveStep(runId, workerId, name, true);
        log.info("Sleep completed", { step: name, ms: durationMs });
        return;
      }
      throw new DeferSignal(new Date(wakeAt));
    },

    async waitFor<T = unknown>(name: string, opts?: StepWaitOptions): Promise<T | null> {
      if (Object.prototype.hasOwnProperty.call(stepState, name)) {
        log.debug("Step cached", { step: name });
        return stepState[name] as T | null;
      }
      const timeoutAt = opts?.timeout != null ? new Date(Date.now() + opts.timeout) : null;
      throw new WaitSignal(name, name, timeoutAt);
    },
  };
}
