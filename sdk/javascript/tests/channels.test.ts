import { afterEach, describe, expect, mock, test } from "bun:test";
import { Channel, ChannelRegistry } from "../src/channels";
import { configureChannels, resetChannelsConfig } from "../src/channels/configure";
import { SseReader } from "../src/channels/sse-reader";
import { defineChannel } from "../src/channels/define";
import { expectAsyncToThrow } from "./assertions";

describe("channels", () => {
  afterEach(() => {
    mock.restore();
    resetChannelsConfig();
  });

  test("creates channel handles with a name", () => {
    const channel = new Channel("chat/room-123");
    expect(channel.name).toBe("chat/room-123");
  });

  test("authorizes a registered exact channel", async () => {
    const reg = new ChannelRegistry();
    reg.register(
      "chat",
      defineChannel("chat", {
        auth: {
          verify(input) {
            expect(input.channel).toBe("chat");
            expect(input.operation).toBe("subscribe");
            return true;
          },
        },
      }),
    );

    const result = await reg.authorize({
      channel: "chat",
      operation: "subscribe",
      params: { roomId: "room-123" },
    });

    expect(result.ok).toBe(true);
  });

  test("channel lookup is exact by registered name", async () => {
    const reg = new ChannelRegistry();
    reg.register(
      "chat",
      defineChannel("chat", {
        auth: { verify: async () => ({ subject: "user-123" }) },
      }),
    );

    const result = await reg.authorize({
      channel: "chat",
      operation: "subscribe",
      params: { roomId: "room-123" },
    });

    expect(result).toEqual({
      ok: true,
      replayWindowMs: 600_000,
      inactivityTtlMs: 0,
      keepaliveIntervalMs: 25_000,
      maxConnectionLifetimeMs: 7_200_000,
      subject: "user-123",
    });
  });

  test("publish without the server runtime points browser clients to websockets", async () => {
    const channel = new Channel("chat/room-123");
    await expectAsyncToThrow(
      () =>
        channel.publish(
          { type: "message", data: { text: "hi" } },
          { baseUrl: "https://app.example.com" },
        ),
      /connect\(\)\.send/,
    );
  });

  test("subscribe opens the canonical SSE route", () => {
    const eventSourceFactory = mock((url: string) => ({ url, kind: "eventsource", close() {} }));
    const webSocketFactory = mock((url: string) => ({ url, kind: "websocket" }));
    const channel = new Channel("chat", undefined, { roomId: "room-123" });

    const subscription = channel.subscribe({
      baseUrl: "https://app.example.com",
      eventSourceFactory,
      webSocketFactory,
    });

    expect(subscription.transport).toBe("sse");
    expect(subscription.raw).toEqual({
      kind: "eventsource",
      url: "https://app.example.com/_tako/channels/chat?roomId=room-123",
      close: expect.any(Function),
    });
    expect(eventSourceFactory).toHaveBeenCalledTimes(1);
    expect(webSocketFactory).toHaveBeenCalledTimes(0);
  });

  test("subscribe encodes the whole channel name as one flat route segment", () => {
    const eventSourceFactory = mock((url: string) => ({ url, kind: "eventsource", close() {} }));
    const channel = new Channel("chat/room-123");

    channel.subscribe({
      baseUrl: "https://app.example.com",
      eventSourceFactory,
    });

    expect(eventSourceFactory).toHaveBeenCalledWith(
      "https://app.example.com/_tako/channels/chat%2Froom-123",
      {},
    );
  });

  test("subscribe uses fetch-based SSE reader by default", async () => {
    const fetchMock = mock((_url: string) =>
      Promise.resolve(
        new Response("data: hi\n\n", {
          status: 200,
          headers: { "Content-Type": "text/event-stream" },
        }),
      ),
    );
    configureChannels({ fetch: fetchMock as unknown as typeof fetch });

    const channel = new Channel("chat", undefined, { roomId: "room-123" });
    const subscription = channel.subscribe({
      baseUrl: "https://app.example.com",
      headers: { Authorization: "Bearer t" },
      lastEventId: "7",
    });

    expect(subscription.transport).toBe("sse");
    expect(subscription.raw).toBeInstanceOf(SseReader);
    await (subscription.raw as SseReader).drain({ connections: 1 });

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = fetchMock.mock.calls[0]!;
    expect(url).toBe("https://app.example.com/_tako/channels/chat?roomId=room-123");
    expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer t");
    expect(new Headers(init?.headers).get("Last-Event-ID")).toBe("7");
  });

  test("subscribe keeps fetch-based SSE alive after the stream ends", async () => {
    const seen: Array<string | null> = [];
    configureChannels({
      fetch: mock((_url: string, init?: RequestInit) => {
        seen.push(new Headers(init?.headers).get("Last-Event-ID"));
        return Promise.resolve(
          new Response(`id: ${seen.length}\ndata: hi\n\n`, {
            headers: { "Content-Type": "text/event-stream" },
          }),
        );
      }) as unknown as typeof fetch,
    });

    const channel = new Channel("chat");
    const subscription = channel.subscribe({ baseUrl: "https://app.example.com" });
    await (subscription.raw as SseReader).drain({ connections: 2 });

    expect(seen).toEqual([null, "1"]);
  });

  test("connect targets the canonical websocket route with last_message_id", () => {
    const send = mock((_data: unknown) => {});
    const close = mock((_code?: number, _reason?: string) => {});
    const webSocketFactory = mock((url: string) => ({ url, kind: "websocket", send, close }));
    const channel = new Channel("chat", "ws", { roomId: "room-123" });

    const connection = channel.connect({
      baseUrl: "https://app.example.com",
      lastMessageId: "42",
      webSocketFactory,
    });

    expect(connection.transport).toBe("ws");
    expect(connection.raw).toEqual({
      kind: "websocket",
      url: "wss://app.example.com/_tako/channels/chat?roomId=room-123&last_message_id=42",
      send,
      close,
    });

    connection.send({ type: "typing" });
    connection.close(1000, "done");

    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith(JSON.stringify({ type: "typing" }));
    expect(close).toHaveBeenCalledTimes(1);
  });

  test("connect uses configured websocket when no factory is passed", () => {
    class MockWebSocket {
      readonly url: string;
      sent: unknown[] = [];

      constructor(url: string) {
        this.url = url;
      }

      send(data: unknown) {
        this.sent.push(data);
      }

      close() {}
    }
    configureChannels({ websocket: MockWebSocket as unknown as typeof WebSocket });

    const channel = new Channel("chat", "ws", { roomId: "room-123" });
    const connection = channel.connect({ baseUrl: "https://app.example.com" });

    expect(connection.raw).toBeInstanceOf(MockWebSocket);
    expect((connection.raw as MockWebSocket).url).toBe(
      "wss://app.example.com/_tako/channels/chat?roomId=room-123",
    );
  });

  test("connect sends tako.auth envelope when token is configured", async () => {
    const sent: unknown[] = [];
    const webSocketFactory = mock(() => ({
      readyState: 1,
      OPEN: 1,
      send: (data: unknown) => sent.push(data),
      close() {},
    }));
    configureChannels({ token: () => "Bearer abc" });

    const channel = new Channel("chat", "ws");
    channel.connect({
      baseUrl: "https://app.example.com",
      lastMessageId: "42",
      webSocketFactory,
    });
    await Promise.resolve();
    await Promise.resolve();

    expect(sent[0]).toBe(
      JSON.stringify({ type: "tako.auth", token: "Bearer abc", lastMessageId: "42" }),
    );
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
      defineChannel("chat", {
        auth: { verify: async () => ({ subject: "user-123" }) },
        handler: { msg: async (d) => d },
        replayWindowMs: 86_400_000,
        inactivityTtlMs: 0,
        keepaliveIntervalMs: 25_000,
        maxConnectionLifetimeMs: 7_200_000,
      }),
    );

    const result = await reg.authorize({
      channel: "chat",
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
