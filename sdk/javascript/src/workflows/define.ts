import { TakoError } from "../tako/error";
import type { EnqueueOptions } from "./engine";
import type { RunId, WorkflowOpts, WorkflowRuntimeOpts } from "./types";
import type { WorkflowHandler } from "./worker";

/** Internal marker attached to workflow definitions. */
export const WORKFLOW_SYMBOL = Symbol("workflow");

/** Runtime metadata attached to a workflow export. */
export interface WorkflowDefinition<P = unknown> {
  /** Internal marker for workflow definitions. */
  readonly type: typeof WORKFLOW_SYMBOL;
  /** Declared workflow name. */
  readonly name: string;
  /** Function that executes one workflow run. */
  readonly handler: WorkflowHandler<P>;
  /** Runtime workflow options without the handler. */
  readonly opts: WorkflowRuntimeOpts;
}

/**
 * The default export from a `<app_root>/workflows/<name>.ts` file. `.enqueue(payload)`
 * schedules a run; `.definition` holds the discovery metadata.
 */
export interface WorkflowExport<P = unknown> {
  /** Discovery metadata consumed by the Tako runtime. */
  readonly definition: WorkflowDefinition<P>;
  /** Schedule a run of this workflow with the declared payload type. */
  enqueue(payload: P, options?: EnqueueOptions): Promise<RunId>;
}

/**
 * Runtime hooks for workflow enqueue and signal. The server/worker
 * bootstrap installs these at boot; client bundles never install them,
 * so `.enqueue()` and `signal()` throw a clean `TakoError` if reached
 * from browser code (same failure shape as a missing Tako server).
 */
export interface WorkflowRuntime {
  /** Enqueue a workflow run. */
  enqueue(name: string, payload: unknown, options?: EnqueueOptions): Promise<RunId>;
  /** Wake workflow runs parked on an event. */
  signal(event: string, payload?: unknown): Promise<number>;
}

let runtime: WorkflowRuntime | null = null;

/**
 * Install the workflow runtime. Called once at server/worker boot — keeps
 * `defineWorkflow`, `.enqueue`, and `signal` free of any static import
 * chain into the RPC client (and its `node:net` dep), so authoring files
 * stay safe to bundle into isomorphic code.
 */
export function setWorkflowRuntime(rt: WorkflowRuntime | null): void {
  runtime = rt;
}

function requireRuntime(): WorkflowRuntime {
  if (!runtime) {
    throw new TakoError(
      "TAKO_UNAVAILABLE",
      "Workflow runtime not installed. `.enqueue()` and `signal()` can only be called from server-side code.",
    );
  }
  return runtime;
}

/**
 * Define a workflow and return a typed handle ready to enqueue.
 *
 * The `name` must be unique per app — the conventional choice is the file
 * basename (kebab-case), matching the filename discovery scans for.
 *
 * @example
 * ```ts
 * // <app_root>/workflows/send-email.ts
 * import { defineWorkflow } from "tako.sh";
 *
 * export default defineWorkflow<{ userId: string }>(
 *   "send-email",
 *   {
 *     retries: 4,
 *     schedule: "0 9 * * *",
 *     handler: async (payload, ctx) => {
 *       ctx.logger.info("sending email");
 *       await ctx.run("send", (step) => {
 *         step.logger.info("calling mailer");
 *         return sendEmail(payload.userId);
 *       });
 *     },
 *   },
 * );
 *
 * // anywhere:
 * import sendEmail from "./workflows/send-email";
 * await sendEmail.enqueue({ userId: "u1" });
 * ```
 */
export function defineWorkflow<P = unknown>(
  name: string,
  opts: WorkflowOpts<P>,
): WorkflowExport<P> {
  const { handler, ...runtimeOpts } = opts;
  const definition: WorkflowDefinition<P> = {
    type: WORKFLOW_SYMBOL,
    name,
    handler,
    opts: runtimeOpts,
  };
  return {
    definition,
    enqueue(payload, options) {
      return requireRuntime().enqueue(name, payload, options);
    },
  };
}

/**
 * Wake every workflow run parked on `ctx.waitFor(event)` with a payload.
 * Call from any server-side context — an HTTP handler, a webhook receiver,
 * a cron tick, another workflow. Returns the number of waiters woken.
 *
 * @example
 * ```ts
 * import { signal } from "tako.sh";
 * await signal(`approval:order-${orderId}`, { approved: true });
 * ```
 */
export function signal(event: string, payload?: unknown): Promise<number> {
  return requireRuntime().signal(event, payload);
}

/** Narrow `value` to a `WorkflowExport` produced by `defineWorkflow`. */
export function isWorkflowExport(value: unknown): value is WorkflowExport {
  return (
    typeof value === "object" &&
    value !== null &&
    "definition" in value &&
    isWorkflowDefinition((value as { definition: unknown }).definition)
  );
}

/** Narrow `value` to a `WorkflowDefinition` produced by `defineWorkflow`. */
export function isWorkflowDefinition(value: unknown): value is WorkflowDefinition {
  return (
    typeof value === "object" &&
    value !== null &&
    "type" in value &&
    "name" in value &&
    "handler" in value &&
    "opts" in value &&
    (value as { type: unknown }).type === WORKFLOW_SYMBOL
  );
}
