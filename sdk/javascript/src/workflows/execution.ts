import { AsyncLocalStorage } from "node:async_hooks";
import { randomUUID } from "node:crypto";
import { DeferSignal } from "./step";
import { setWorkflowExecutionResolver, type DurableWorkflowExecution } from "./result";
import type { WorkflowsClient } from "./rpc-client";
import type { StepState } from "./types";

const storage = new AsyncLocalStorage<Execution>();
setWorkflowExecutionResolver(() => storage.getStore());

class Execution implements DurableWorkflowExecution {
  private sequence = 0;
  private readonly used = new Set<string>();
  constructor(
    private readonly client: WorkflowsClient,
    private readonly runId: string,
    private readonly owner: string,
    private readonly state: StepState,
    private readonly scope: string,
    readonly pending: Set<Promise<unknown>> = new Set(),
  ) {}
  key(step?: string): string {
    const key = `__workflow:${JSON.stringify([this.scope, step ?? this.sequence++])}`;
    if (this.used.has(key)) throw new Error(`Duplicate durable workflow step: ${step}`);
    this.used.add(key);
    return key;
  }
  newChildId(): string {
    // Saved before enqueueing. Reusing a pruned parent ID creates a new child,
    // even when the previous incarnation's child history is still retained.
    return `child:${randomUUID()}`;
  }
  async checkpoint<T>(key: string, fn: () => Promise<T>): Promise<T> {
    if (Object.prototype.hasOwnProperty.call(this.state, key)) return this.state[key] as T;
    const value = await fn();
    await this.client.saveStep(this.runId, this.owner, key, value);
    this.state[key] = value;
    return value;
  }
  defer(wakeAt: Date): never {
    throw new DeferSignal(wakeAt);
  }
  track<T>(fn: () => Promise<T>): Promise<T> {
    const promise = fn();
    this.pending.add(promise);
    void promise.then(
      () => this.pending.delete(promise),
      () => this.pending.delete(promise),
    );
    return promise;
  }
  withinStep<T>(name: string, fn: () => T): T {
    return storage.run(
      new Execution(
        this.client,
        this.runId,
        this.owner,
        this.state,
        `${this.scope}/${JSON.stringify(name)}`,
        this.pending,
      ),
      fn,
    );
  }
}

/** Each named ctx.run has its own call sequence, so skipped cached steps don't shift later calls. */
export function withinWorkflowStep<T>(name: string, fn: () => T): T {
  const context = storage.getStore();
  return context ? context.withinStep(name, fn) : fn();
}

export async function withWorkflowExecution<T>(
  client: WorkflowsClient,
  runId: string,
  owner: string,
  state: StepState,
  fn: () => T,
): Promise<Awaited<T>> {
  const execution = new Execution(client, runId, owner, state, "");
  try {
    return await storage.run(execution, fn);
  } finally {
    // Promise.all can reject with a deferral while sibling calls are still
    // saving checkpoints. Finish those writes before releasing the lease.
    while (execution.pending.size) await Promise.allSettled([...execution.pending]);
  }
}
