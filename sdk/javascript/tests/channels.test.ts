import { afterEach, describe, expect, mock, test } from "bun:test";
import { Channel, ChannelRegistry } from "../src/channels";
import { defineChannel } from "../src/channels/define";

describe("channels", () => {
  afterEach(() => {
    mock.restore();
  });

  test("creates channel handles with a name", () => {
    const channel = new Channel("chat/room-123");
    expect(channel.name).toBe("chat/room-123");
  });

  test("authorizes a registered exact channel", async () => {
    const reg = new ChannelRegistry();
    reg.register(
      "chat",
      defineChannel("chat/room-123", {
        auth(_req, ctx) {
          expect(ctx.channel).toBe("chat/room-123");
          expect(ctx.operation).toBe("subscribe");
          return true;
        },
      }),
    );

    const result = await reg.authorize({
      channel: "chat/room-123",
      operation: "subscribe",
      params: {},
    });

    expect(result.ok).toBe(true);
  });

  test("most specific pattern wins over param capture", async () => {
    const reg = new ChannelRegistry();
    reg.register("chat-prefix", defineChannel("chat/:roomId", { auth: async () => false }));
    reg.register(
      "chat-exact",
      defineChannel("chat/room-123", { auth: async () => ({ subject: "user-123" }) }),
    );

    const result = await reg.authorize({
      channel: "chat/room-123",
      operation: "subscribe",
      params: { roomId: "room-123" },
    });

    expect(result).toEqual({
      ok: true,
      replayWindowMs: 86_400_000,
      inactivityTtlMs: 0,
      keepaliveIntervalMs: 25_000,
      maxConnectionLifetimeMs: 7_200_000,
      subject: "user-123",
    });
  });

  test("publish routes through HTTP when no socket publisher is installed", async () => {
    const fetchMock = mock(() =>
      Promise.resolve(
        new Response(JSON.stringify({ id: "42", channel: "chat/room-123" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );
    const originalFetch = globalThis.fetch;
    globalThis.fetch = fetchMock as typeof fetch;

    try {
      const channel = new Channel("chat/room-123");
      const response = await channel.publish(
        { type: "message", data: { text: "hi" } },
        { baseUrl: "https://app.example.com" },
      );

      expect(response.id).toBe("42");
      expect(fetchMock).toHaveBeenCalledTimes(1);

      const [url, init] = fetchMock.mock.calls[0]!;
      expect(url).toBe("https://app.example.com/channels/chat/room-123/messages");
      expect(init?.method).toBe("POST");
      expect(init?.headers).toEqual({ "Content-Type": "application/json" });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  test("subscribe opens the canonical SSE route", () => {
    const eventSourceFactory = mock((url: string) => ({ url, kind: "eventsource", close() {} }));
    const webSocketFactory = mock((url: string) => ({ url, kind: "websocket" }));
    const channel = new Channel("chat/room-123");

    const subscription = channel.subscribe({
      baseUrl: "https://app.example.com",
      eventSourceFactory,
      webSocketFactory,
    });

    expect(subscription.transport).toBe("sse");
    expect(subscription.raw).toEqual({
      kind: "eventsource",
      url: "https://app.example.com/channels/chat/room-123",
      close: expect.any(Function),
    });
    expect(eventSourceFactory).toHaveBeenCalledTimes(1);
    expect(webSocketFactory).toHaveBeenCalledTimes(0);
  });

  test("connect targets the canonical websocket route with last_message_id", () => {
    const send = mock((_data: unknown) => {});
    const close = mock((_code?: number, _reason?: string) => {});
    const webSocketFactory = mock((url: string) => ({ url, kind: "websocket", send, close }));
    const channel = new Channel("chat/room-123", "ws");

    const connection = channel.connect({
      baseUrl: "https://app.example.com",
      lastMessageId: "42",
      webSocketFactory,
    });

    expect(connection.transport).toBe("ws");
    expect(connection.raw).toEqual({
      kind: "websocket",
      url: "wss://app.example.com/channels/chat/room-123?last_message_id=42",
      send,
      close,
    });

    connection.send({ type: "typing" });
    connection.close(1000, "done");

    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith(JSON.stringify({ type: "typing" }));
    expect(close).toHaveBeenCalledTimes(1);
  });

  test("connect throws when channel has no ws transport", () => {
    const channel = new Channel("status");
    expect(() => channel.connect({ baseUrl: "https://app.example.com" })).toThrow(
      /does not enable WebSocket/,
    );
  });

  test("authorize stamps lifecycle config from definition", async () => {
    const reg = new ChannelRegistry();
    reg.register(
      "chat",
      defineChannel<{ msg: { text: string } }>("chat/:roomId", {
        auth: async () => ({ subject: "user-123" }),
        handler: { msg: async (d) => d },
        replayWindowMs: 86_400_000,
        inactivityTtlMs: 0,
        keepaliveIntervalMs: 25_000,
        maxConnectionLifetimeMs: 7_200_000,
      }),
    );

    const result = await reg.authorize({
      channel: "chat/room-123",
      operation: "subscribe",
      params: { roomId: "room-123" },
    });

    expect(result).toEqual({
      ok: true,
      subject: "user-123",
      replayWindowMs: 86_400_000,
      inactivityTtlMs: 0,
      keepaliveIntervalMs: 25_000,
      maxConnectionLifetimeMs: 7_200_000,
      transport: "ws",
    });
  });
});
