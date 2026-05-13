/**
 * tako.sh — authoring helpers for channels, workflows, and typed errors.
 *
 * ```ts
 * import { tako } from "tako.sh";
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

import { createLogger } from "./logger";
import { loadSecrets } from "./tako/secrets";

/**
 * Project-specific secret keys. Augmented by the generated `tako.d.ts` file.
 */
export interface TakoSecrets {}

/**
 * Project-specific type registry. Augmented by the generated `tako.d.ts` file.
 */
export interface TakoTypeRegistry {}

/**
 * Channel metadata discovered from `<app_root>/channels/`. Augmented by the
 * generated `tako.d.ts` file.
 */
export interface TakoChannels {}

/** Environments declared in `tako.toml`, plus `development` and `production`. */
export type Env = TakoTypeRegistry extends { Env: infer T extends string }
  ? T
  : "development" | "production";

/** Redaction helpers available on `tako.secrets`. */
export interface TakoSecretRedactions {
  /** `String(tako.secrets)` returns `"[REDACTED]"` to prevent accidental leaks. */
  toString(): "[REDACTED]";
  /** `JSON.stringify(tako.secrets)` returns `"[REDACTED]"` to prevent accidental leaks. */
  toJSON(): "[REDACTED]";
}

/** Tako-managed secret bag with project-specific keys from `tako.d.ts`. */
export type TakoSecretBag = Readonly<TakoSecrets> & TakoSecretRedactions;

const __takoEnv: Record<string, string> =
  typeof process !== "undefined" && process.env
    ? (process.env as Record<string, string>)
    : ({} as Record<string, string>);

/** Current Tako environment. */
export const env = (__takoEnv["ENV"] ?? "") as Env;

/** `true` when the app is running under `tako dev`. */
export const isDev = env === "development";

/** `true` when the app is running under `tako deploy` in a production env. */
export const isProd = env === "production";

/** Port Tako assigned to this app instance. */
export const port = Number(__takoEnv["PORT"] ?? 0);

/** Host/address Tako bound this app instance to. */
export const host = __takoEnv["HOST"] ?? "";

/** Build identifier injected by Tako. `"dev"` under `tako dev`. */
export const build = __takoEnv["TAKO_BUILD"] ?? "";

/** Persistent app-owned data directory. */
export const dataDir = __takoEnv["TAKO_DATA_DIR"] ?? "";

/** Directory the app is running from (`process.cwd()`). */
export const appDir =
  typeof process !== "undefined" && typeof process.cwd === "function" ? process.cwd() : "";

/** Structured JSON logger bound to `source: "app"`. */
export const logger = createLogger("app");

/** Tako-managed secrets, typed by project-specific `tako.d.ts` declarations. */
export const secrets = loadSecrets<TakoSecretBag>();

/** Primary app runtime surface. */
export const tako = Object.freeze({
  env,
  isDev,
  isProd,
  port,
  host,
  build,
  dataDir,
  appDir,
  logger,
  secrets,
} as const);

/** Type of the exported {@link tako} runtime object. */
export type TakoRuntime = typeof tako;

export { defineChannel } from "./channels/define";
export { defineWorkflow, signal } from "./workflows/define";
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

type BoundChannel<T> = T extends (params: never) => infer Handle ? Handle : T;

/** Extract the params type from a channel definition export. */
type InferChannelParams<T> =
  BoundChannel<T> extends { readonly __params?: infer P } ? P : Record<string, never>;

/** Extract the message map from a channel definition export. */
type InferChannelMessages<T> =
  BoundChannel<T> extends { readonly __messages?: infer M } ? M : Record<string, unknown>;

/** Extract the client transport from a channel definition export. */
type InferChannelTransport<T> =
  BoundChannel<T> extends {
    connect(options?: import("./types").ChannelConnectOptions): import("./types").ChannelSocket;
  }
    ? "ws"
    : "sse";

/**
 * Infer the typed registry entry for a channel definition export.
 *
 * Used by generated `tako.d.ts` channel metadata; also useful when writing
 * wrappers around project-local channel modules.
 */
export type InferChannel<T> = {
  params: InferChannelParams<T>;
  messages: InferChannelMessages<T>;
  transport: InferChannelTransport<T>;
};
