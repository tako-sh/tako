/**
 * Stable error codes raised by the Tako internal-RPC layer. Apps can switch
 * on `err.code` to render a user-safe message; the original cause is on
 * `err.cause` (and logged to stdout so operators can debug).
 */
export type TakoErrorCode =
  | "TAKO_UNAVAILABLE"
  | "TAKO_TIMEOUT"
  | "TAKO_PROTOCOL"
  | "TAKO_RPC_ERROR";

/**
 * Error raised by Tako SDK operations that cross the internal socket
 * (workflows enqueue/signal/claim/..., channels publish). Every raw Node
 * socket failure is wrapped so internal paths and syscall names never leak
 * to end users — `message` stays generic, `code` is stable, and the
 * original error is preserved on `.cause`.
 */
export class TakoError extends Error {
  /** Stable machine-readable error code. */
  readonly code: TakoErrorCode;

  /**
   * Create a Tako SDK error.
   *
   * @param code - Stable machine-readable error code.
   * @param message - User-safe error message.
   * @param options - Optional underlying cause.
   */
  constructor(code: TakoErrorCode, message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "TakoError";
    this.code = code;
  }
}
