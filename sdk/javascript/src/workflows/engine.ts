/**
 * WorkflowEngine — SDK-facing surface for durable tasks.
 *
 * This module is imported by **two processes**:
 *
 * 1. **Worker process** (`bunx tako-worker`, etc.) — uses
 *    `discover/register/startWorker/drain`. All DB ops go over RPC to
 *    tako-server via `WorkflowsClient`.
 *
 * 2. **HTTP app process** — `enqueue` delegates to the same RPC client.
 *    The SDK never opens SQLite; tako-server owns the queue file.
 *
 * The same singleton supports both — callers just invoke the relevant
 * subset.
 */

import { discoverWorkflows, type WorkflowDiscoveryOptions } from "./discovery";
import { WorkflowsClient } from "./rpc-client";
import type { WorkflowRuntimeOpts } from "./types";
import type { RunId } from "./types";
import { Worker, type RegisteredWorkflow, type WorkflowHandler } from "./worker";

interface Registration {
  handler: WorkflowHandler;
  opts: WorkflowRuntimeOpts;
}

/** Options for `workflow.enqueue(payload, options)`. */
export interface EnqueueOptions {
  /**
   * When to run.
   * @defaultValue now
   */
  runAt?: Date;
  /**
   * Number of retries after the first attempt. Overrides the workflow-level default.
   * @defaultValue workflow's configured retries
   */
  retries?: number;
  /**
   * Uniqueness key. If a non-terminal run with this key already exists,
   * enqueue is a no-op and the existing run id is returned.
   */
  uniqueKey?: string | null;
}

/**
 * Runtime coordinator for workflow discovery, enqueue, signal, and worker execution.
 *
 * Application code normally uses `defineWorkflow(...).enqueue(...)` and
 * `signal(...)`; this class is exported for Tako runtime internals.
 */
export class WorkflowEngine {
  private client: WorkflowsClient | null = null;
  private worker: Worker | null = null;
  private workerId = "";
  private configuredFlag = false;
  private readonly registrations = new Map<string, Registration>();

  /** True once configure() has succeeded (worker process only). */
  get configured(): boolean {
    return this.configuredFlag;
  }

  /** The workflow names that have been registered (worker process only). */
  get registeredNames(): string[] {
    return Array.from(this.registrations.keys());
  }

  /** Worker-process setup. Attaches the RPC client + worker identity. */
  configure(opts: { client: WorkflowsClient; workerId: string }): void {
    if (this.configuredFlag) throw new Error("WorkflowEngine already configured");
    this.client = opts.client;
    this.workerId = opts.workerId;
    this.configuredFlag = true;
  }

  /**
   * HTTP-process setup. Explicitly attach a client (tests inject a mock).
   * If not called, the engine lazily tries `WorkflowsClient.fromEnv()` on
   * first enqueue.
   */
  setClient(client: WorkflowsClient | null): void {
    this.client = client;
  }

  /**
   * Register one workflow handler.
   *
   * @param name - Workflow name.
   * @param handler - Workflow function.
   * @param opts - Runtime workflow options.
   * @defaultValue opts = {}
   */
  register(name: string, handler: WorkflowHandler, opts: WorkflowRuntimeOpts = {}): void {
    if (this.registrations.has(name)) {
      throw new Error(`workflow '${name}' is already registered`);
    }
    this.registrations.set(name, { handler, opts });
  }

  /**
   * Scan `dir` for workflow files and register each one by filename (without
   * extension). `opts.worker` can narrow discovery to workflows assigned to a
   * specific worker group.
   */
  async discover(dir: string, opts: WorkflowDiscoveryOptions = {}): Promise<number> {
    const found = await discoverWorkflows(dir, opts);
    for (const entry of found) {
      this.register(entry.name, entry.handler, entry.opts);
    }
    return found.length;
  }

  /**
   * Enqueue a task. In the HTTP process this goes through the per-app
   * enqueue socket (tako-server writes to SQLite). In the worker process
   * (where a backend is configured) it still goes through the RPC path —
   * the worker doesn't self-enqueue via its own DB handle, which keeps
   * server ownership of cron-dedup idempotent.
   */
  async enqueue(name: string, payload: unknown, opts: EnqueueOptions = {}): Promise<RunId> {
    const client = this.resolveClient();
    const effectiveOpts: EnqueueOptions = { ...opts };
    if (effectiveOpts.retries === undefined) {
      const reg = this.registrations.get(name);
      if (reg?.opts.retries !== undefined) {
        effectiveOpts.retries = reg.opts.retries;
      }
    }
    const result = await client.enqueue(name, payload, effectiveOpts);
    return result.id;
  }

  /** Deliver an event payload to every parked waitFor matching `eventName`. */
  async signal(eventName: string, payload?: unknown): Promise<number> {
    return this.resolveClient().signal(eventName, payload ?? null);
  }

  private resolveClient(): WorkflowsClient {
    if (!this.client) {
      this.client = WorkflowsClient.fromEnv();
    }
    if (!this.client) {
      throw new Error(
        "Workflow engine has no RPC client. Set TAKO_INTERNAL_SOCKET + TAKO_APP_NAME or call setClient().",
      );
    }
    return this.client;
  }

  /** Start the worker with default runtime options. */
  start(): void {
    this.startWorker({});
  }

  /**
   * Worker-process start with runtime-provided concurrency / idle timeout.
   * Called by the worker entrypoint bootstrap; user code normally uses
   * `start()` which accepts no arguments.
   */
  startWorker(opts: { concurrency?: number; idleTimeoutMs?: number }): void {
    if (this.worker) return;
    const client = this.resolveClient();
    const registry = new Map<string, RegisteredWorkflow>();
    for (const [name, reg] of this.registrations) {
      const entry: RegisteredWorkflow = { handler: reg.handler };
      if (reg.opts.retries !== undefined || reg.opts.backoff !== undefined) {
        entry.retry = {};
        if (reg.opts.retries !== undefined) entry.retry.maxAttempts = reg.opts.retries + 1;
        if (reg.opts.backoff !== undefined) entry.retry.backoff = reg.opts.backoff;
      }
      registry.set(name, entry);
    }
    this.worker = new Worker({
      client,
      registry,
      workerId: this.workerId,
      ...(opts.concurrency !== undefined && { concurrency: opts.concurrency }),
      ...(opts.idleTimeoutMs !== undefined && { idleTimeoutMs: opts.idleTimeoutMs }),
    });
    this.worker.start();
  }

  /** True if the running worker exited because it went idle. */
  get workerIdled(): boolean {
    return this.worker?.idled ?? false;
  }

  /** Stop claiming new runs and wait for active runs to finish. */
  async drain(): Promise<void> {
    if (this.worker) {
      await this.worker.drain();
      this.worker = null;
    }
  }

  /** Number of workflow runs currently in flight. */
  running(): number {
    return this.worker?.runningCount ?? 0;
  }

  /** Gather registered cron schedules for `RegisterSchedules`. */
  collectSchedules(): Array<{ name: string; cron: string }> {
    const out: Array<{ name: string; cron: string }> = [];
    for (const [name, reg] of this.registrations) {
      if (reg.opts.schedule) {
        out.push({ name, cron: reg.opts.schedule });
      }
    }
    return out;
  }

  /** Test-only: reset all state. */
  _reset(): void {
    this.client = null;
    this.worker = null;
    this.workerId = "";
    this.configuredFlag = false;
    this.registrations.clear();
  }
}

/** Singleton exported on the global Tako object. */
export const workflowsEngine = new WorkflowEngine();
