/**
 * Browser-safe runtime surface used by framework/runtime internals.
 *
 * Only exposes symbols whose module graph is free of `node:*` imports.
 * Server-adapter surface (`handleTakoEndpoint`,
 * `initServerRuntime`) lives on `tako.sh/internal` — do not re-export it
 * from here.
 */

export { createLogger, Logger } from "./logger";
export type { Logger as LoggerType } from "./logger";
export { loadSecrets } from "./tako/secrets";
