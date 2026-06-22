import type { ChannelRegistry } from "../channels";

/** Wire frame received from a WebSocket channel client. */
export interface WsFrame {
  /** Application-defined message kind. */
  type: string;
  /** Message payload. */
  data: unknown;
}

/** Input used to dispatch one client WebSocket message. */
export interface DispatchInput {
  /** Exact channel name. */
  channel: string;
  /** Bound channel params. */
  params?: Record<string, unknown>;
  /** Parsed client frame. */
  frame: WsFrame;
  /** Authenticated subject, when available. */
  subject?: string;
}

/** Dispatch outcome consumed by the channel server. */
export type DispatchResult =
  | { action: "fanout"; data: unknown }
  | { action: "drop"; error?: string }
  | { action: "reject"; reason: string };

/**
 * Run the matching WebSocket channel message handler and report whether the
 * server should fan out, drop, or reject the frame.
 */
export async function dispatchWsMessage(
  registry: ChannelRegistry,
  input: DispatchInput,
): Promise<DispatchResult> {
  const matched = registry.resolve(input.channel);
  if (!matched) return { action: "reject", reason: "channel_not_defined" };

  const definition = matched.definition;
  if (definition.handler === undefined) {
    return { action: "reject", reason: "sse_channel_not_writable" };
  }

  const fn = definition.handler[input.frame.type as keyof typeof definition.handler];
  if (typeof fn !== "function") {
    return { action: "fanout", data: input.frame.data };
  }

  try {
    const result = await fn(input.frame.data as never, {
      channel: input.channel,
      operation: "publish",
      params: input.params ?? {},
      ...(input.subject !== undefined && { subject: input.subject }),
      publishedBy: "client",
    });
    if (result === undefined || result === null) return { action: "drop" };
    return { action: "fanout", data: result };
  } catch (err) {
    return { action: "drop", error: err instanceof Error ? err.message : String(err) };
  }
}
