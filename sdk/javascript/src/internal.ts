/**
 * tako.sh/internal — server-only plumbing.
 *
 * Two audiences:
 *   - The generated `tako.gen.ts` file, which pulls `loadSecrets` and
 *     `createLogger` from here.
 *   - Framework-adapter authors wiring Tako into a new host (custom Vite
 *     plugin, Next.js middleware, edge adapter), who use
 *     `handleTakoEndpoint` to answer Tako's internal protocol requests.
 *
 * Not for app code. `loadSecrets` reads from an fd pipe and `createLogger`
 * writes JSON to `process.stdout`; do not import from here in browser
 * code. Client-side apps import from `tako.sh/client`; app server code
 * imports `workflowsEngine` and authoring helpers from `tako.sh`.
 *
 * Keep this surface narrow — anything re-exported here becomes
 * statically reachable from consumer bundlers, and side-effectful
 * bindings (like singletons) pull their source modules in even when the
 * named import isn't used.
 */

export type {
  WorkflowDefinition,
  WorkflowHandler,
  WorkflowContext,
  WorkflowStepContext,
  WorkflowOpts,
  WorkflowRuntimeOpts,
} from "./workflows";
export type { WorkflowExport } from "./workflows/define";
export type { StepRunOptions, StepWaitOptions } from "./workflows";
export { isWorkflowDefinition, isWorkflowExport } from "./workflows";

export type {
  ChannelConnectOptions,
  ChannelDefinitionTransport,
  ChannelGrant,
  ChannelMessage,
  ChannelPublishInput,
  ChannelPublishOptions,
  ChannelSocket,
  ChannelSubscribeOptions,
  ChannelSubscription,
  FetchHandler,
} from "./types";
export type {
  ChannelDefinition,
  ChannelAuthConfig,
  ChannelExport,
  ChannelHandle,
  VerifyInput,
} from "./channels/define";
export { defineChannel, isChannelDefinition, isChannelExport } from "./channels/define";

export { loadSecrets } from "./tako/secrets";
export { createLogger } from "./logger";
export type { Logger } from "./logger";

export { handleTakoEndpoint } from "./tako/endpoints";
export { normalizeFetchResponse } from "./tako/fetch-response";
export type { TakoStatus } from "./types";

export { initServerRuntime } from "./tako/init";
