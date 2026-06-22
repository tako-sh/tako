import type {
  ChannelHeaderValue,
  ChannelMessage,
  ChannelOperation,
  ChannelPublishOptions,
} from "../types";

/** Internal marker attached to channel definitions. */
export const CHANNEL_SYMBOL = Symbol.for("tako.channel");

/** Return value accepted from a channel auth callback. */
export type ChannelAuthResult = boolean | { subject?: string };

/** Input passed to a channel auth callback. */
export interface VerifyInput<Params = Record<string, unknown>> {
  /** Exact channel name being authorized. */
  channel: string;
  /** Operation being authorized. */
  operation: ChannelOperation;
  /** Params validated from the channel query string. */
  params: Params;
  /** Header credential extracted by tako-server. */
  header?: ChannelHeaderValue;
  /** Cookie credential extracted by tako-server. */
  cookie?: string;
}

/** Declarative channel auth configuration. */
export interface ChannelAuthConfig<Params> {
  /**
   * Header name to read before calling `verify`; set to `false` for cookie-only auth.
   * @defaultValue "authorization"
   */
  headerName?: string | false;
  /** Cookie name to read before calling `verify`. */
  cookieName?: string;
  /** Auth callback for subscribe, publish, and connect operations. */
  verify: (input: VerifyInput<Params>) => ChannelAuthResult | Promise<ChannelAuthResult>;
}

/** Context passed to WebSocket message handlers. */
export interface ChannelHandlerContext<Params = Record<string, unknown>> {
  /** Exact channel name handling the message. */
  channel: string;
  /** Operation being handled. */
  operation: ChannelOperation;
  /** Bound channel params. */
  params: Params;
  /** Authenticated subject returned by `verify`, when present. */
  subject?: string;
  /** Whether the message came from server-side publish or a connected client. */
  publishedBy: "server" | "client";
}

/** Handler for one WebSocket message type. */
export type MessageHandler<Data, Params> = (
  data: Data,
  ctx: ChannelHandlerContext<Params>,
) => Data | void | Promise<Data | void>;

/** Lifecycle knobs controlling replay, idle eviction, and keepalives per channel. */
export interface ChannelLifecycleConfig {
  /** @defaultValue 600_000 (10 min) */
  replayWindowMs?: number;
  /** @defaultValue 0 (no inactivity eviction) */
  inactivityTtlMs?: number;
  /** @defaultValue 25_000 (25 s) */
  keepaliveIntervalMs?: number;
  /** @defaultValue 7_200_000 (2 h) */
  maxConnectionLifetimeMs?: number;
}

/** Auth configuration stored on a channel definition. */
export type ChannelAuthScheme<Params> =
  | false
  | {
      /**
       * Header name to read before calling `verify`; set to `false` for cookie-only auth.
       * @defaultValue "authorization"
       */
      headerName?: string | false;
      /** Cookie name to read before calling `verify`. */
      cookieName?: string;
      /** Auth callback for subscribe, publish, and connect operations. */
      verify: ChannelAuthConfig<Params>["verify"];
    };

/** Runtime metadata attached to every channel export. */
export interface ChannelDefinition<
  Params = Record<string, unknown>,
  Messages = Record<string, unknown>,
> extends ChannelLifecycleConfig {
  /** Internal marker for channel definitions. */
  readonly type: typeof CHANNEL_SYMBOL;
  /** Exact channel name. */
  readonly channel: string;
  /** JSON Schema used by tako-server to validate params. */
  readonly paramsSchema: object;
  /** Auth policy for this channel. */
  readonly auth: ChannelAuthScheme<Params>;
  /** Optional WebSocket message handlers. Presence enables WebSocket transport. */
  readonly handler?: { [T in keyof Messages]?: MessageHandler<Messages[T], Params> };
  /** Live transport exposed to clients when handlers are present. */
  readonly transport?: "ws";
  /** Whether this channel requires params before use. */
  readonly hasParams: boolean;
}

/** Runtime handle used by channel discovery internals. */
export interface ChannelHandle<Params, Messages> {
  /** Type-only params marker. */
  readonly __params?: Params;
  /** Channel name plus encoded params, useful for diagnostics. */
  readonly name: string;
  /** Publish a typed message to subscribers. */
  publish<T extends keyof Messages & string>(
    message: { type: T; data: Messages[T] },
    options?: ChannelPublishOptions,
  ): Promise<ChannelMessage<Messages[T]>>;
}

/** Metadata attached to channel module exports. */
export interface ChannelExportMeta<Params, Messages> {
  /** Runtime channel definition consumed by Tako discovery. */
  readonly definition: ChannelDefinition<Params, Messages>;
}

/** Narrow `value` to an object with channel export metadata. */
export function isChannelExport(value: unknown): value is { definition: ChannelDefinition } {
  return (
    value !== null &&
    (typeof value === "function" || typeof value === "object") &&
    "definition" in (value as object) &&
    isChannelDefinition((value as { definition: unknown }).definition)
  );
}

/** Narrow `value` to a channel definition. */
export function isChannelDefinition(value: unknown): value is ChannelDefinition {
  return (
    typeof value === "object" &&
    value !== null &&
    "type" in value &&
    (value as { type: unknown }).type === CHANNEL_SYMBOL
  );
}
