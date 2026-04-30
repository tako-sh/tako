import type { ChannelRegistry } from "../channels";

export interface WsFrame {
  type: string;
  data: unknown;
}

export interface DispatchInput {
  channel: string;
  params?: Record<string, unknown>;
  frame: WsFrame;
  subject?: string;
}

export type DispatchResult =
  | { action: "fanout"; data: unknown }
  | { action: "drop"; error?: string }
  | { action: "reject"; reason: string };

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
