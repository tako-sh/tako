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
 * Why the server rejected a request, on a `TAKO_RPC_ERROR` that carries one.
 * The server sends a reason only where app or SDK code may need to branch;
 * other rejections leave `reason` undefined.
 *
 * - `run_not_owned`: a workflow worker write names a run this attempt no longer owns.
 * - `run_id_conflict`: `enqueue({ id })` reused an ID that belongs to a different workflow or payload.
 * - `invalid_request`: the request or its arguments are malformed; retrying it cannot succeed.
 * - `channel_unknown`: the channel is not defined by this build or by the recipient app.
 * - `channel_params_invalid`: channel params are malformed or fail the channel's params schema.
 * - `channel_closed`: the channel stream was closed and accepts no more messages.
 * - `unavailable`: the server is draining, overloaded, or its storage is unreachable; retry later.
 */
export type TakoErrorReason =
  | "run_not_owned"
  | "run_id_conflict"
  | "invalid_request"
  | "channel_unknown"
  | "channel_params_invalid"
  | "channel_closed"
  | "unavailable";

const REASONS: ReadonlySet<string> = new Set<TakoErrorReason>([
  "run_not_owned",
  "run_id_conflict",
  "invalid_request",
  "channel_unknown",
  "channel_params_invalid",
  "channel_closed",
  "unavailable",
]);

/**
 * The reason a server error reply carries, or undefined for an unclassified
 * reply or a code this SDK version does not know.
 *
 * @internal
 */
export function takoErrorReason(code: unknown): TakoErrorReason | undefined {
  return typeof code === "string" && REASONS.has(code) ? (code as TakoErrorReason) : undefined;
}

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

  /** Why the server rejected the request, when `code` is `TAKO_RPC_ERROR` and the server said. */
  readonly reason: TakoErrorReason | undefined;

  /**
   * Create a Tako SDK error.
   *
   * @param code - Stable machine-readable error code.
   * @param message - User-safe error message.
   * @param options - Optional underlying cause and server rejection reason.
   */
  constructor(
    code: TakoErrorCode,
    message: string,
    options?: { cause?: unknown; reason?: TakoErrorReason },
  ) {
    super(message, options);
    this.name = "TakoError";
    this.code = code;
    this.reason = options?.reason;
  }
}
