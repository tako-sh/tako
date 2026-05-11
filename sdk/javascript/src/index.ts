/**
 * tako.sh — authoring helpers for channels, workflows, and typed errors.
 *
 * Runtime state is imported from the generated `tako.gen.ts` file in each
 * project as the app-specific `tako` object, not from this package:
 *
 * ```ts
 * import { tako } from "../tako.gen";
 * import sendEmail from "../workflows/send-email";
 * import missionLog from "../channels/mission-log";
 *
 * tako.logger.info("boot", { env: tako.env });
 * const dbUrl = tako.secrets.DATABASE_URL;
 * await sendEmail.enqueue({ to: "u@e.co" });
 * await missionLog({ base }).publish({ type: "event", data });
 * ```
 *
 * There is no `Tako` global — Tako v0 uses plain ES modules for everything.
 */

export { defineChannel } from "./channels/define";
export { defineWorkflow, signal } from "./workflows/define";
export {
  createImageUrl,
  type CreateImageUrlOptions,
  type ImageCrop,
  type ImageFit,
  type PrivateImageUrlOptions,
  type PublicImageUrlOptions,
} from "./images";
export type {
  EnqueueOptions,
  WorkflowContext,
  WorkflowOpts,
  WorkflowStepContext,
} from "./workflows";
export { TakoError, type TakoErrorCode } from "./tako/error";

/**
 * Extract the payload type from a workflow definition.
 * Rarely needed directly — `defineWorkflow<P>(...)` already constrains
 * `.enqueue(payload)`. Useful when wrapping enqueue in generic helpers.
 *
 * @example
 * ```ts
 * type P = InferWorkflowPayload<typeof import("./workflows/send-email").default>;
 * ```
 */
export type InferWorkflowPayload<T> = T extends import("./workflows").WorkflowExport<infer P>
  ? P
  : T extends import("./workflows").WorkflowDefinition<infer P>
    ? P
    : unknown;
