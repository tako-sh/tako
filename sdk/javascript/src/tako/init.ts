/**
 * Shared server-runtime initialization.
 *
 * Every context that hosts a Tako server-side user module — production
 * server entry (`createEntrypoint.run`), Vite dev plugin, future edge
 * adapters — installs the same publisher + runtime registrations so that
 * `Channel.publish()`, `signal()`, and `defineWorkflow(...).enqueue()`
 * all work when invoked from app code.
 *
 * Worker entrypoints call `setWorkflowRuntime` separately because they
 * first need to configure `workflowsEngine` with an explicit
 * `WorkflowsClient` identity.
 */

import { setWorkflowRuntime } from "../workflows/define";
import { workflowsEngine } from "../workflows/engine";
import { assertInternalSocketEnvConsistency, installChannelSocketPublisherFromEnv } from "./socket";

/**
 * Install server-side runtime hooks for channels and workflows.
 *
 * @internal Called by framework adapters and entrypoints before user code
 * invokes runtime helpers.
 */
export function initServerRuntime(): void {
  assertInternalSocketEnvConsistency();
  installChannelSocketPublisherFromEnv();
  setWorkflowRuntime({
    enqueue: (name, payload, opts) => workflowsEngine.enqueue(name, payload, opts),
    signal: (event, payload) => workflowsEngine.signal(event, payload),
  });
}
