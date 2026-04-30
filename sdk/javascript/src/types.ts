/**
 * Tako SDK Types
 */

/**
 * Standard web `fetch` handler signature.
 *
 * Compatible with Cloudflare Workers, Deno Deploy, Bun, Node's undici-based
 * `Request`/`Response`, and other runtimes exposing the Fetch API. Tako
 * passes its secrets bag as the second argument.
 */
export type FetchFunction = (
  request: Request,
  env: Record<string, string>,
) => Response | Promise<Response>;

/** Alias of {@link FetchFunction}. */
export type FetchHandler = FetchFunction;

/**
 * A {@link FetchFunction} that optionally exposes a readiness hook.
 *
 * If present, Tako awaits `ready()` before marking the instance healthy and
 * routing traffic to it — use this to gate on warmup work (cache prefill,
 * config fetch, etc.).
 */
export interface ReadyableFetchHandler extends FetchFunction {
  /** Optional warmup hook. Tako awaits this before readiness is signaled. */
  ready?: () => void | Promise<void>;
}

/** Transport modes that a channel definition can opt into. */
export type ChannelDefinitionTransport = "ws";
/** Transport modes a live channel session can use. */
export type ChannelLiveTransport = "sse" | "ws";
/** Channel operations that pass through the auth callback. */
export type ChannelOperation = "subscribe" | "publish" | "connect";

/** Declared header credential extracted by tako-server before verify. */
export interface ChannelHeaderValue {
  /** Optional auth scheme, e.g. `"Bearer"`. */
  scheme?: string;
  /** Credential value without the scheme prefix when a scheme is present. */
  value: string;
}

/**
 * Successful auth payload returned from a channel's `auth` callback.
 *
 * Return `true` as shorthand for `{}` when no subject is needed.
 */
export interface ChannelGrant {
  /** Stable identifier for the authenticated subject (user id, api key, etc.). */
  subject?: string;
}

/**
 * What a channel's `auth` callback may return.
 *
 * `false` denies; `true` allows anonymously; a {@link ChannelGrant} allows and
 * records a `subject`. Can be returned synchronously or as a `Promise`.
 */
export type ChannelAuthResult = boolean | ChannelGrant | Promise<boolean | ChannelGrant>;

/** Lifecycle knobs controlling replay, idle eviction, and keepalives per channel. */
export interface ChannelLifecycleConfig {
  /** @defaultValue 86_400_000 (24 h) */
  replayWindowMs?: number;
  /** @defaultValue 0 (no inactivity eviction) */
  inactivityTtlMs?: number;
  /** @defaultValue 25_000 (25 s) */
  keepaliveIntervalMs?: number;
  /** @defaultValue 7_200_000 (2 h) */
  maxConnectionLifetimeMs?: number;
}

/** Input to {@link import("./channels").ChannelRegistry.authorize}. */
export interface ChannelAuthorizeInput {
  /** Exact channel name being authorized. */
  channel: string;
  /** Operation being requested. */
  operation: ChannelOperation;
  /** JSON params validated by tako-server from the channel query string. */
  params: Record<string, unknown>;
  /** Declared header credential, when the channel auth scheme requested one. */
  header?: ChannelHeaderValue;
  /** Declared cookie credential, when the channel auth scheme requested one. */
  cookie?: string;
}

/**
 * Result of authorizing a channel request — what the Tako server uses to
 * decide whether to open the stream and with what lifecycle config.
 */
export interface ChannelAuthorizeResponse extends ChannelGrant, ChannelLifecycleConfig {
  /** `true` when the request is authorized. */
  ok: boolean;
  /** Echoes the definition's transport when applicable. */
  transport?: ChannelDefinitionTransport;
  /** Machine-readable rejection reason when `ok` is `false`. */
  reason?: string;
}

/** Shape of a message as published by app code. */
export interface ChannelPublishInput<T = unknown> {
  /** Application-defined message kind (e.g. `"chat.send"`). */
  type: string;
  /** Message payload. */
  data: T;
}

/** Persisted channel message with server-assigned id and channel name. */
export interface ChannelMessage<T = unknown> extends ChannelPublishInput<T> {
  /** Server-assigned monotonic message id. */
  id: string;
  /** The exact channel the message was published on. */
  channel: string;
}

/** Common HTTP-shaped options for channel requests. */
export interface ChannelRequestOptions {
  /** Override the base URL. Required outside the browser. */
  baseUrl?: string;
  /** Extra headers to send. */
  headers?: Record<string, string>;
  /** AbortSignal to cancel the request. */
  signal?: AbortSignal;
}

/** Init argument accepted by a custom {@link ChannelSubscribeOptions.eventSourceFactory}. */
export interface EventSourceFactoryInit {
  /** Extra headers to apply to the SSE request (factory permitting). */
  headers?: Record<string, string>;
  /** Last seen message id to resume from. */
  lastEventId?: string;
}

/** Options for {@link import("./channels").Channel.subscribe}. */
export interface ChannelSubscribeOptions {
  /** Override the base URL. Required outside the browser. */
  baseUrl?: string;
  /** Extra headers to send on the SSE request. */
  headers?: Record<string, string>;
  /** Last seen message id — triggers replay from that point. */
  lastEventId?: string;
  /** Inject a custom `EventSource` implementation (e.g. for Node, tests). */
  eventSourceFactory?: (url: string, init?: EventSourceFactoryInit) => unknown;
}

/** Options for {@link import("./channels").Channel.connect}. */
export interface ChannelConnectOptions {
  /** Override the base URL. Required outside the browser. */
  baseUrl?: string;
  /** Extra headers to send on the upgrade request (factory permitting). */
  headers?: Record<string, string>;
  /** Last seen message id — replays missed frames before live traffic. */
  lastMessageId?: string;
  /** Inject a custom `WebSocket` implementation (e.g. for Node, tests). */
  webSocketFactory?: (url: string) => unknown;
}

/** Options for {@link import("./channels").Channel.publish}. */
export interface ChannelPublishOptions extends ChannelRequestOptions {}

/** Handle returned by {@link import("./channels").Channel.subscribe}. */
export interface ChannelSubscription {
  /** Always `"sse"` — subscriptions are server-sent events. */
  transport: "sse";
  /** The underlying `EventSource` (or factory result). Escape hatch for advanced callers. */
  raw: unknown;
  /** End the subscription. */
  close: () => void;
}

/** Handle returned by {@link import("./channels").Channel.connect}. */
export interface ChannelSocket {
  /** Always `"ws"` — sockets are WebSockets. */
  transport: "ws";
  /** The underlying `WebSocket` (or factory result). Escape hatch for advanced callers. */
  raw: unknown;
  /** Close the socket with an optional code/reason. */
  close: (code?: number, reason?: string) => void;
  /** Send a payload. Objects are JSON-stringified; binary types are passed through. */
  send: (data: unknown) => void;
}

/**
 * Response body from the Tako SDK's built-in `/status` endpoint
 * (served on the `tako.internal` host).
 */
export interface TakoStatus {
  /** Current lifecycle state of the instance. */
  status: "healthy" | "starting" | "draining" | "unhealthy";
  /** Application name. */
  app: string;
  /** Deployed build version. */
  version: string;
  /** Stable identifier for this running instance. */
  instance_id: string;
  /** Process id. */
  pid: number;
  /** Seconds since the instance started. */
  uptime_seconds: number;
}
