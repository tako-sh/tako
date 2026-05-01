import type {
  ChannelAuthorizeInput,
  ChannelAuthorizeResponse,
  ChannelConnectOptions,
  ChannelDefinitionTransport,
  ChannelMessage,
  ChannelPublishInput,
  ChannelPublishOptions,
  ChannelSocket,
  ChannelSubscribeOptions,
  ChannelSubscription,
} from "./types";
import { isChannelDefinition, type ChannelDefinition } from "./channels/meta";
import { getChannelsConfig } from "./channels/configure";
import { SseReader } from "./channels/sse-reader";

export type ChannelSocketPublisher = <T>(
  channel: string,
  message: ChannelPublishInput<T>,
) => Promise<ChannelMessage<T>>;

let socketPublisher: ChannelSocketPublisher | null = null;

export function setChannelSocketPublisher(fn: ChannelSocketPublisher | null): void {
  socketPublisher = fn;
}

export const TAKO_CHANNELS_BASE_PATH = "/channels";
const DEFAULT_CHANNEL_REPLAY_WINDOW_MS = 24 * 60 * 60 * 1000;
const DEFAULT_CHANNEL_INACTIVITY_TTL_MS = 0;
const DEFAULT_CHANNEL_KEEPALIVE_INTERVAL_MS = 25 * 1000;
const DEFAULT_CHANNEL_MAX_CONNECTION_LIFETIME_MS = 2 * 60 * 60 * 1000;

function normalizeBaseUrl(baseUrl?: string): URL {
  if (baseUrl) {
    return new URL(baseUrl);
  }
  if (typeof globalThis.location?.origin === "string" && globalThis.location.origin.length > 0) {
    return new URL(globalThis.location.origin);
  }
  throw new Error("Channel operations require a baseUrl outside the browser.");
}

function channelBaseUrl(
  channel: string,
  baseUrl?: string,
  params: Record<string, unknown> = {},
): URL {
  const url = normalizeBaseUrl(baseUrl);
  url.pathname = `${TAKO_CHANNELS_BASE_PATH}/${encodeURIComponent(channel)}`;
  url.search = "";
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null) continue;
    url.searchParams.set(key, encodeQueryValue(value));
  }
  return url;
}

function encodeQueryValue(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") {
    return value.toString();
  }
  return JSON.stringify(value);
}

function withQuery(url: URL, key: string, value?: string | number): URL {
  if (value !== undefined) {
    url.searchParams.set(key, String(value));
  }
  return url;
}

function toWebSocketUrl(url: URL): string {
  const ws = new URL(url.toString());
  ws.protocol = ws.protocol === "https:" ? "wss:" : "ws:";
  return ws.toString();
}

function defaultWebSocketFactory(url: string): unknown {
  const ctor = getChannelsConfig().websocket;
  if (!ctor) {
    throw new Error("WebSocket is not available in this runtime.");
  }
  return new ctor(url);
}

function closeRaw(raw: unknown, code?: number, reason?: string): void {
  if (typeof raw !== "object" || raw === null) {
    return;
  }
  const maybeClose = (raw as { close?: (code?: number, reason?: string) => void }).close;
  if (typeof maybeClose === "function") {
    maybeClose.call(raw, code, reason);
  }
}

function sendRaw(raw: unknown, data: unknown): void {
  if (typeof raw !== "object" || raw === null) {
    throw new Error("Channel connection does not support send().");
  }
  const maybeSend = (raw as { send?: (data: unknown) => void }).send;
  if (typeof maybeSend !== "function") {
    throw new Error("Channel connection does not support send().");
  }
  let payload = data;
  if (
    data !== null &&
    typeof data === "object" &&
    !(data instanceof ArrayBuffer) &&
    !ArrayBuffer.isView(data) &&
    !(typeof Blob !== "undefined" && data instanceof Blob)
  ) {
    payload = JSON.stringify(data);
  }
  maybeSend.call(raw, payload);
}

function sendWhenOpen(raw: unknown, data: unknown): void {
  if (typeof raw !== "object" || raw === null) return;
  const target = raw as {
    readyState?: number;
    OPEN?: number;
    addEventListener?: (type: "open", listener: () => void, opts?: { once?: boolean }) => void;
  };
  const send = () => sendRaw(raw, data);
  const openState = target.OPEN ?? 1;

  if (target.readyState === undefined || target.readyState === openState) {
    send();
    return;
  }

  if (typeof target.addEventListener === "function") {
    target.addEventListener("open", send, { once: true });
  }
}

function sendAuthEnvelope(raw: unknown, lastMessageId?: string): void {
  void getChannelsConfig()
    .resolveOptionalToken()
    .then((token) => {
      if (!token) return;
      sendWhenOpen(raw, {
        type: "tako.auth",
        token,
        ...(lastMessageId !== undefined && { lastMessageId }),
      });
    });
}

export class Channel {
  readonly name: string;
  readonly transport: ChannelDefinitionTransport | undefined;
  readonly params: Record<string, unknown>;

  constructor(
    name: string,
    transport?: ChannelDefinitionTransport,
    params: Record<string, unknown> = {},
  ) {
    this.name = name;
    this.transport = transport;
    this.params = params;
  }

  async publish<T = unknown>(
    message: ChannelPublishInput<T>,
    options: ChannelPublishOptions = {},
  ): Promise<ChannelMessage<T>> {
    if (socketPublisher && !options.baseUrl) {
      return socketPublisher(this.name, message);
    }

    throw new Error(
      "Channel.publish requires the Tako server runtime. Browser clients should use connect().send() for WebSocket channels.",
    );
  }

  subscribe(options: ChannelSubscribeOptions = {}): ChannelSubscription {
    const url = channelBaseUrl(this.name, options.baseUrl, this.params);
    const factory = options.eventSourceFactory;
    const init: { headers?: Record<string, string>; lastEventId?: string } = {};
    if (options.headers !== undefined) {
      init.headers = options.headers;
    }
    if (options.lastEventId !== undefined) {
      init.lastEventId = options.lastEventId;
    }
    if (factory) {
      const raw = factory(url.toString(), init);
      return {
        transport: "sse",
        raw,
        close() {
          closeRaw(raw);
        },
      };
    }

    const readerOptions = {
      fetch: getChannelsConfig().fetch,
      onMessage: () => {},
      ...(options.headers !== undefined && { headers: options.headers }),
    };
    const reader = new SseReader(url.toString(), readerOptions);
    if (options.lastEventId !== undefined) {
      reader.lastEventId = options.lastEventId;
    }
    void reader.start();
    return {
      transport: "sse",
      raw: reader,
      close() {
        reader.close();
      },
    };
  }

  connect(options: ChannelConnectOptions = {}): ChannelSocket {
    if (this.transport !== "ws") {
      throw new Error("Channel does not enable WebSocket transport.");
    }

    const url = channelBaseUrl(this.name, options.baseUrl, this.params);
    withQuery(url, "last_message_id", options.lastMessageId);

    const factory = options.webSocketFactory ?? defaultWebSocketFactory;
    const raw = factory(toWebSocketUrl(url));
    sendAuthEnvelope(raw, options.lastMessageId);
    return {
      transport: "ws",
      raw,
      close(code?: number, reason?: string) {
        closeRaw(raw, code, reason);
      },
      send(data: unknown) {
        sendRaw(raw, data);
      },
    };
  }
}

interface RegistryEntry {
  name: string;
  definition: ChannelDefinition;
}

/**
 * Handle returned by the default export of a `channels/<name>.ts` file
 * (unparameterized) or by invoking a parameterized channel with its params.
 */
export interface ChannelHandle {
  publish: Channel["publish"];
  subscribe: Channel["subscribe"];
  connect?: Channel["connect"];
}

/**
 * Loose runtime shape for the default export of a channel module.
 * Unparameterized channels expose `publish/subscribe/connect` directly;
 * parameterized channels are callable `(params)` returning a handle. Typed
 * as an intersection so both usages compile — runtime enforces which one
 * is valid for a given channel.
 */
export type ChannelAccessorEntry = ChannelHandle &
  ((params: Record<string, unknown>) => ChannelHandle);

function makeHandle(
  definition: ChannelDefinition,
  resolvedName: string,
  params: Record<string, unknown> = {},
): ChannelHandle {
  const isWs = definition.transport === "ws";
  const channel = new Channel(resolvedName, isWs ? "ws" : undefined, params);
  const handle: ChannelHandle = {
    publish: channel.publish.bind(channel),
    subscribe: channel.subscribe.bind(channel),
  };
  if (isWs) {
    handle.connect = channel.connect.bind(channel);
  }
  return handle;
}

function buildAccessorEntry(definition: ChannelDefinition, baseName: string): ChannelAccessorEntry {
  if (!definition.hasParams) {
    return makeHandle(definition, baseName) as ChannelAccessorEntry;
  }
  return ((params: Record<string, unknown>) =>
    makeHandle(definition, baseName, params)) as ChannelAccessorEntry;
}

/** Convert a camelCase prop to the kebab-case channel file name. */
function propToChannelName(prop: string): string {
  return prop
    .replace(/([a-z])([A-Z])/g, "$1-$2")
    .replace(/([a-zA-Z])([0-9])/g, "$1-$2")
    .replace(/([0-9])([a-zA-Z])/g, "$1-$2")
    .toLowerCase();
}

export class ChannelRegistry {
  private entries: RegistryEntry[] = [];

  get all(): ReadonlyArray<RegistryEntry> {
    return this.entries;
  }

  register(
    name: string,
    input: ChannelDefinition | { readonly definition: ChannelDefinition },
  ): void {
    const definition: ChannelDefinition =
      "definition" in input && isChannelDefinition(input.definition)
        ? input.definition
        : (input as ChannelDefinition);
    if (this.entries.some((e) => e.name === name)) {
      throw new Error(`duplicate channel '${name}'`);
    }
    if (definition.channel !== name) {
      throw new Error(
        `channel '${name}' registration received definition for '${definition.channel}'`,
      );
    }
    this.entries.push({ name, definition });
  }

  clear(): void {
    this.entries = [];
  }

  /** Look up a discovered channel by its declared channel name. */
  findByName(name: string): RegistryEntry | undefined {
    return this.entries.find((e) => e.name === name);
  }

  resolve(
    channel: string,
  ): { definition: ChannelDefinition; params: Record<string, unknown> } | null {
    const entry = this.entries.find((candidate) => candidate.name === channel);
    if (!entry) return null;
    return { definition: entry.definition, params: {} };
  }

  async authorize(input: ChannelAuthorizeInput): Promise<ChannelAuthorizeResponse> {
    const matched = this.resolve(input.channel);
    if (!matched) return { ok: false };

    if (input.operation === "publish" && matched.definition.handler === undefined) {
      return { ok: false, reason: "sse_publish_not_allowed" };
    }

    if (matched.definition.auth === false) {
      return { ok: true, ...definitionLifecycleConfig(matched.definition) };
    }

    const verdict = await matched.definition.auth.verify({
      channel: input.channel,
      operation: input.operation,
      params: input.params,
      ...(input.header !== undefined && { header: input.header }),
      ...(input.cookie !== undefined && { cookie: input.cookie }),
    });

    if (verdict === false) return { ok: false };

    const config = definitionLifecycleConfig(matched.definition);
    if (verdict === true) return { ok: true, ...config };
    return verdict.subject === undefined
      ? { ok: true, ...config }
      : { ok: true, ...config, subject: verdict.subject };
  }
}

/**
 * Wrap a {@link ChannelRegistry} in a Proxy so property access via
 * `accessor.<name>` returns a {@link ChannelAccessorEntry} for the matching
 * discovered channel (the prop is kebab-cased first). Existing registry
 * methods (`register`, `resolve`, `authorize`, `clear`, `all`, `findByName`)
 * pass through.
 */
export function withChannelAccessor(
  registry: ChannelRegistry,
): ChannelRegistry & Record<string, ChannelAccessorEntry> {
  const handler: ProxyHandler<ChannelRegistry> = {
    get(target, prop, receiver) {
      if (typeof prop === "string" && !(prop in target)) {
        const entry = target.findByName(propToChannelName(prop));
        if (entry) {
          return buildAccessorEntry(entry.definition, entry.name);
        }
      }
      return Reflect.get(target, prop, receiver);
    },
  };
  return new Proxy(registry, handler) as ChannelRegistry & Record<string, ChannelAccessorEntry>;
}

function definitionLifecycleConfig(definition: ChannelDefinition) {
  const config: Omit<ChannelAuthorizeResponse, "ok" | "subject" | "reason"> = {
    replayWindowMs: definition.replayWindowMs ?? DEFAULT_CHANNEL_REPLAY_WINDOW_MS,
    inactivityTtlMs: definition.inactivityTtlMs ?? DEFAULT_CHANNEL_INACTIVITY_TTL_MS,
    keepaliveIntervalMs: definition.keepaliveIntervalMs ?? DEFAULT_CHANNEL_KEEPALIVE_INTERVAL_MS,
    maxConnectionLifetimeMs:
      definition.maxConnectionLifetimeMs ?? DEFAULT_CHANNEL_MAX_CONNECTION_LIFETIME_MS,
  };
  if (definition.handler !== undefined) {
    config.transport = "ws";
  }
  return config;
}
