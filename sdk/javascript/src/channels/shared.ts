import { getChannelsConfig, onChannelConnectionsReset } from "./configure";

const MAX_FRAME_BYTES = 64 * 1024;
const MAX_MESSAGE_BYTES = 1024 * 1024;
const MAX_BUFFERED_BYTES = 1024 * 1024;
const MAX_SUBSCRIPTIONS = 32;
const encoder = new TextEncoder();
// Keep events in the same realm as the EventTarget base class.
const ChannelEvent = Event;
const connections = new Map<string, SharedConnection>();
const factories = new WeakMap<object, number>();
let nextFactory = 0;
let nextSubscription = 0;

export interface SharedOptions {
  headers?: Record<string, string>;
  cursor?: string;
  from?: string;
  webSocketFactory?: (url: string) => unknown;
}

function event(type: string, fields: Record<string, unknown> = {}): Event {
  return Object.assign(new ChannelEvent(type), fields);
}

class SendError extends Error {
  constructor(
    message: string,
    readonly code: number,
  ) {
    super(message);
  }
}

/** A page-owned logical socket. Closing it releases only this subscription. */
export class SharedChannel extends EventTarget {
  readonly id = String(++nextSubscription);
  readonly OPEN = 1;
  readyState = 0;
  private chunks: string[] = [];
  private bytes = 0;
  private chunkTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly connection: SharedConnection,
    readonly channel: string,
    readonly query: string,
    readonly options: SharedOptions,
  ) {
    super();
  }

  open(): void {
    if (this.readyState !== 0) return;
    this.readyState = 1;
    this.dispatchEvent(event("open"));
  }

  receive(data: unknown, end: unknown): void {
    if (this.readyState !== 1) return;
    if (typeof data !== "string" || typeof end !== "boolean") {
      this.fail(new Error("Invalid channel message fragment."));
      return;
    }
    this.bytes += encoder.encode(data).byteLength;
    if (this.bytes > MAX_MESSAGE_BYTES) {
      this.fail(new Error("Channel message exceeds 1 MiB."), 1009);
      return;
    }
    this.chunks.push(data);
    clearTimeout(this.chunkTimer);
    if (!end) {
      this.chunkTimer = setTimeout(
        () => this.fail(new Error("Incomplete channel message.")),
        30_000,
      );
      return;
    }
    const message = this.chunks.join("");
    this.chunks = [];
    this.bytes = 0;
    this.dispatchEvent(event("message", { data: message }));
  }

  send(data: unknown): void {
    if (this.readyState !== 1) throw new Error("Channel subscription is not open.");
    const value = typeof data === "string" ? (JSON.parse(data) as unknown) : data;
    this.connection.send({ type: "send", id: this.id, data: value });
  }

  close(code = 1000, reason = "unsubscribed"): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    clearTimeout(this.chunkTimer);
    this.chunks = [];
    this.bytes = 0;
    this.connection.release(this);
    this.dispatchEvent(event("close", { code, reason }));
  }

  fail(error: Error, code = 1002): void {
    this.dispatchEvent(event("error", { error }));
    this.close(code, error.message);
  }
}

class SharedConnection {
  readonly subscriptions = new Map<string, SharedChannel>();
  private readonly socket: WebSocket;
  private disposed = false;
  private idle: ReturnType<typeof setTimeout> | undefined;
  private readonly config = getChannelsConfig();

  constructor(
    private readonly key: string,
    url: string,
    factory: (url: string) => unknown,
  ) {
    this.socket = factory(url) as WebSocket;
    if (typeof this.socket?.addEventListener !== "function") {
      throw new Error("Channel WebSocket must implement addEventListener().");
    }
    this.socket.addEventListener("open", () => {
      for (const subscription of this.subscriptions.values()) void this.join(subscription);
    });
    this.socket.addEventListener("message", (message: MessageEvent<unknown>) => {
      if (this.disposed) return;
      try {
        if (
          typeof message.data !== "string" ||
          encoder.encode(message.data).byteLength > MAX_FRAME_BYTES
        ) {
          throw new Error("Invalid or oversized shared channel frame.");
        }
        const frame = JSON.parse(message.data) as {
          id?: unknown;
          type?: unknown;
          data?: unknown;
          end?: unknown;
          code?: unknown;
          reason?: unknown;
        };
        if (!frame || typeof frame.id !== "string")
          throw new Error("Invalid shared channel frame.");
        const subscription = this.subscriptions.get(frame.id);
        if (!subscription) return; // A page may have left while data was in flight.
        switch (frame.type) {
          case "subscribed":
            subscription.open();
            break;
          case "data":
            subscription.receive(frame.data, frame.end);
            break;
          case "closed":
            if (typeof frame.code !== "number" || typeof frame.reason !== "string")
              throw new Error("Invalid subscription close.");
            subscription.close(frame.code, frame.reason);
            break;
          case "unsubscribed":
            break;
          default:
            throw new Error("Unknown shared channel frame.");
        }
      } catch (error) {
        this.shutdown(
          1002,
          error instanceof Error ? error.message : "Invalid shared channel frame",
        );
      }
    });
    this.socket.addEventListener("close", (closed: CloseEvent) =>
      this.shutdown(closed.code, closed.reason),
    );
    this.socket.addEventListener("error", () => this.shutdown(1006, "Channel connection failed"));
  }

  acquire(channel: string, query: string, options: SharedOptions): SharedChannel {
    if (this.subscriptions.size >= MAX_SUBSCRIPTIONS)
      throw new Error("A shared connection supports at most 32 subscriptions.");
    clearTimeout(this.idle);
    const subscription = new SharedChannel(this, channel, query, options);
    this.subscriptions.set(subscription.id, subscription);
    if (this.socket.readyState === 1) queueMicrotask(() => void this.join(subscription));
    return subscription;
  }

  private async join(subscription: SharedChannel): Promise<void> {
    try {
      const headers = new Headers(subscription.options.headers);
      // Like tako.auth on a dedicated socket, the token is independent of the
      // definition's header name. Explicit declared headers take precedence.
      const token = headers.get("authorization") ?? (await this.config.resolveOptionalToken());
      if (this.disposed || !this.subscriptions.has(subscription.id) || this.socket.readyState !== 1)
        return;
      this.send({
        type: "subscribe",
        id: subscription.id,
        channel: subscription.channel,
        query: subscription.query,
        headers: Object.fromEntries(headers),
        ...(token && { token }),
        ...(subscription.options.cursor !== undefined && { cursor: subscription.options.cursor }),
        ...(subscription.options.from !== undefined && { from: subscription.options.from }),
      });
    } catch (error) {
      if (this.subscriptions.has(subscription.id))
        subscription.fail(
          error instanceof Error ? error : new Error(String(error)),
          error instanceof SendError ? error.code : 4401,
        );
    }
  }

  send(command: unknown): void {
    const encoded = JSON.stringify(command);
    const bytes = encoder.encode(encoded).byteLength;
    if (bytes > MAX_FRAME_BYTES) {
      throw new SendError("Channel command exceeds 64 KiB; send smaller events.", 1009);
    }
    if (this.disposed || this.socket.readyState !== 1)
      throw new SendError("Channel connection is not open.", 1006);
    if (this.socket.bufferedAmount + bytes > MAX_BUFFERED_BYTES) {
      throw new SendError("Channel outgoing queue is full.", 4429);
    }
    this.socket.send(encoded);
  }

  release(subscription: SharedChannel): void {
    if (!this.subscriptions.delete(subscription.id)) return;
    if (!this.disposed && this.socket.readyState === 1) {
      try {
        this.send({ type: "unsubscribe", id: subscription.id });
      } catch {
        this.shutdown(1006, "Unable to release subscription");
      }
    }
    if (this.subscriptions.size === 0 && !this.disposed) {
      // Reuse across synchronous effect cleanup/remount, but never retain an
      // unused connection indefinitely.
      this.idle = setTimeout(() => this.shutdown(1000, "idle"), 0);
    }
  }

  shutdown(code: number, reason: string): void {
    if (this.disposed) return;
    this.disposed = true;
    clearTimeout(this.idle);
    if (connections.get(this.key) === this) connections.delete(this.key);
    for (const subscription of this.subscriptions.values()) subscription.close(code, reason);
    this.socket.close();
  }
}

onChannelConnectionsReset(() => {
  // Snapshot before dispatching close events: callers may reconnect immediately.
  for (const connection of [...connections.values()])
    connection.shutdown(1012, "Channel credentials or configuration changed");
});

/** Pool by origin, credentials, configuration generation and transport factory. */
export function openSharedChannel(url: URL, options: SharedOptions = {}): SharedChannel {
  const config = getChannelsConfig();
  const factoryIdentity = options.webSocketFactory ?? config.websocket;
  let factoryId = factories.get(factoryIdentity);
  if (factoryId === undefined) {
    factoryId = ++nextFactory;
    factories.set(factoryIdentity, factoryId);
  }
  const channel = decodeURIComponent(url.pathname.slice("/_tako/channels/".length));
  const endpoint = new URL("/_tako/channels", url);
  endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:";
  const headers = [...new Headers(options.headers)].sort(([a], [b]) => a.localeCompare(b));
  const key = JSON.stringify([endpoint.href, headers, factoryId, config.generation]);
  let connection = connections.get(key);
  if (!connection) {
    connection = new SharedConnection(
      key,
      endpoint.href,
      options.webSocketFactory ?? ((address) => new config.websocket(address)),
    );
    connections.set(key, connection);
  }
  const query = new URLSearchParams(url.search);
  query.delete("from");
  query.delete("last_message_id");
  return connection.acquire(channel, query.toString(), options);
}
