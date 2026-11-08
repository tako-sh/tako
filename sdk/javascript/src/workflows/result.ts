import type { EnqueueOptions } from "./engine";
import type { RunStatus } from "./types";

export interface WorkflowResultState<T = unknown> {
  status: RunStatus;
  result: T;
  error: string | null;
}

export interface WorkflowResultOptions {
  /** Maximum wait, in milliseconds or a duration such as "30s". Default: no timeout. */
  timeout?: number | `${number}${"ms" | "s" | "m" | "h"}`;
  /** Stable durable step name within the calling workflow. Otherwise uses call order. */
  step?: string;
}
export type WorkflowRunOptions = EnqueueOptions & WorkflowResultOptions;
export type WorkflowOutput<T> = T extends void ? null : T;

export class WorkflowResultError extends Error {
  constructor(
    public readonly runId: string,
    public readonly status: "missing" | "cancelled" | "dead" | "timeout",
    message: string,
  ) {
    super(message);
    this.name = "WorkflowResultError";
  }
}

type ReadResult = (id: string) => Promise<WorkflowResultState | null>;
type Enqueue = (payload: unknown, options: EnqueueOptions) => Promise<string>;

/** @internal Worker-only execution hooks; this module stays safe for isomorphic imports. */
export interface DurableWorkflowExecution {
  key(step?: string): string;
  newChildId(): string;
  checkpoint<T>(key: string, fn: () => Promise<T>): Promise<T>;
  track<T>(fn: () => Promise<T>): Promise<T>;
  defer(wakeAt: Date): never;
}
let execution: () => DurableWorkflowExecution | undefined = () => undefined;
export function setWorkflowExecutionResolver(resolver: typeof execution): void {
  execution = resolver;
}

function timeoutMs(options: WorkflowResultOptions): number | null {
  const value = options.timeout;
  if (value === undefined) return null;
  const match = typeof value === "string" && /^(\d+(?:\.\d+)?)(ms|s|m|h)$/.exec(value);
  const ms =
    typeof value === "number"
      ? value
      : match
        ? Number(match[1]) * ({ ms: 1, s: 1_000, m: 60_000, h: 3_600_000 }[match[2]!] ?? NaN)
        : NaN;
  if (!Number.isFinite(ms) || ms < 0 || !Number.isSafeInteger(Date.now() + ms))
    throw new Error("Workflow result timeout must be a nonnegative duration");
  return ms;
}

function callIdentity(value: unknown): string {
  // The store may change object-key order when it serializes a checkpoint.
  // Compare JSON values rather than the caller's original insertion order.
  const sort = (value: unknown): unknown =>
    Array.isArray(value)
      ? value.map(sort)
      : value && typeof value === "object"
        ? Object.fromEntries(
            Object.entries(value)
              .sort(([a], [b]) => a.localeCompare(b))
              .map(([key, item]) => [key, sort(item)]),
          )
        : value;
  return JSON.stringify(sort(JSON.parse(JSON.stringify(value))));
}

async function awaitOutcome(
  target: string,
  id: string,
  read: ReadResult,
  timeout: number | null,
  context?: DurableWorkflowExecution,
  key?: string,
): Promise<unknown> {
  const checkpoint = <T>(suffix: string, fn: () => Promise<T>) =>
    context ? context.checkpoint(`${key}:${suffix}`, fn) : fn();
  const identity = await checkpoint("identity", async () => ({ target, id }));
  if (identity.target !== target || identity.id !== id)
    throw new Error(
      "Durable workflow call changed during replay; use stable step names and inputs",
    );
  const deadline = await checkpoint("deadline", async () =>
    timeout === null ? null : Date.now() + timeout,
  );
  const outcome = await checkpoint("outcome", async () => {
    const expired = {
      status: "timeout" as const,
      result: null,
      error: "Timed out waiting for workflow result",
    };
    for (;;) {
      if (deadline !== null && Date.now() >= deadline) return expired;
      let timer: ReturnType<typeof setTimeout> | undefined;
      const state = await (
        deadline === null
          ? read(id)
          : Promise.race([
              read(id),
              new Promise<typeof expired>((resolve) => {
                timer = setTimeout(
                  () => resolve(expired),
                  Math.min(2_147_483_647, Math.max(0, deadline - Date.now())),
                );
              }),
            ])
      ).finally(() => {
        if (timer !== undefined) clearTimeout(timer);
      });
      if (!state)
        return {
          status: "missing" as const,
          result: null,
          error: "Workflow run not found or no longer retained",
        };
      if (
        state.status === "succeeded" ||
        state.status === "cancelled" ||
        state.status === "dead" ||
        state.status === "timeout"
      )
        return state;
      if (state.status !== "pending" && state.status !== "running")
        throw new Error("Invalid workflow result response");
      if (deadline !== null && Date.now() >= deadline) return expired;
      const delay = deadline === null ? 1_000 : Math.min(1_000, Math.max(0, deadline - Date.now()));
      if (context) context.defer(new Date(Date.now() + delay));
      await new Promise((resolve) => setTimeout(resolve, delay));
    }
  });
  if (outcome.status === "succeeded") return outcome.result;
  if (outcome.status === "pending" || outcome.status === "running")
    throw new Error("Invalid terminal workflow result");
  throw new WorkflowResultError(id, outcome.status, outcome.error ?? `Workflow ${outcome.status}`);
}

export async function waitForWorkflowResult(
  target: string,
  id: string,
  read: ReadResult,
  options: WorkflowResultOptions = {},
): Promise<unknown> {
  const timeout = timeoutMs(options);
  const context = execution();
  const key = context?.key(options.step);
  const wait = () => awaitOutcome(target, id, read, timeout, context, key);
  return context ? context.track(wait) : wait();
}

/** Enqueue and wait; the worker runtime checkpoints the child identity and outcome. */
export async function runWorkflow(
  target: string,
  payload: unknown,
  enqueue: Enqueue,
  read: ReadResult,
  options: WorkflowRunOptions = {},
): Promise<unknown> {
  const timeout = timeoutMs(options);
  const context = execution();
  if (!context) return awaitOutcome(target, await enqueue(payload, options), read, timeout);
  const key = context.key(options.step);
  return context.track(async () => {
    const description = callIdentity({
      target,
      payload: payload ?? null,
      retries: options.retries,
      runAt: options.runAt,
      id: options.id,
    });
    const saved = await context.checkpoint(`${key}:call`, async () => ({
      description,
      id: options.id ?? context.newChildId(),
    }));
    if (saved.description !== description)
      throw new Error(
        "Durable workflow call changed during replay; use stable step names and inputs",
      );
    const id = await context.checkpoint(`${key}:enqueue`, () =>
      enqueue(payload, { ...options, id: saved.id }),
    );
    return awaitOutcome(target, id, read, timeout, context, key);
  });
}
