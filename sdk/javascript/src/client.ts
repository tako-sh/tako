/**
 * Browser-safe entry point for Tako.
 *
 * Exposes the client side of Tako channels without pulling in any
 * of the server-only modules (secrets reader, workflow RPC client, entrypoint
 * installer) that the default `tako.sh` entry imports.
 *
 * Use this in code that ships to the browser. For server handlers, use the
 * default `tako.sh` entry. For React, use `tako.sh/react`.
 *
 * @example
 * ```typescript
 * import { Channel } from "tako.sh/client";
 *
 * const chat = new Channel("chat:room-123");
 * const sub = chat.subscribe({ headers: { Authorization: `Bearer ${token}` } });
 * ```
 */

export { Channel } from "./channels";
export { configureChannels } from "./channels/configure";

export type {
  ChannelConnectOptions,
  ChannelMessage,
  ChannelPublishInput,
  ChannelPublishOptions,
  ChannelRequestOptions,
  ChannelSocket,
  ChannelSubscribeOptions,
  ChannelSubscription,
  EventSourceFactoryInit,
} from "./types";
