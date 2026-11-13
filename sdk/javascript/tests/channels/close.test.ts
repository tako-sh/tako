import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { Channel, setChannelSocketCloser } from "../../src/channels";
import { configureChannels, resetChannelsConfig } from "../../src/channels/configure";
import { defineChannel } from "../../src/channels/define";
import { SseReader } from "../../src/channels/sse-reader";
import { expectAsyncToThrow } from "../assertions";

function sseResponse(body: string): Response {
  return new Response(body, { headers: { "content-type": "text/event-stream" } });
}

class FakeSocket extends EventTarget {
  sent: unknown[] = [];
  constructor(readonly url: string) {
    super();
  }
  send(data: unknown) {
    this.sent.push(data);
  }
  close() {}
}

function resetChannelState() {
  setChannelSocketCloser(null);
  resetChannelsConfig();
}

beforeEach(resetChannelState);
afterEach(resetChannelState);

describe("Channel.close", () => {
  test("rejects outside the server runtime", async () => {
    await expectAsyncToThrow(() => new Channel("reply").close(), /Browser clients cannot close/);
  });

  test("rejects ephemeral channels before reaching the server", async () => {
    const closer = mock(async () => ({ id: "c", channel: "live", type: "tako.close", data: null }));
    setChannelSocketCloser(closer);
    const live = defineChannel("live", { ephemeral: true });
    await expectAsyncToThrow(() => live.close(), /ephemeral; only durable channel streams/);
    expect(closer).not.toHaveBeenCalled();
  });

  test("selects the stream with publish's params rule and sends the lifecycle", async () => {
    const calls: unknown[] = [];
    setChannelSocketCloser(async (channel, params, lifecycle) => {
      calls.push({ channel, params, lifecycle });
      return { id: "c", channel, type: "tako.close", data: null };
    });
    const reply = defineChannel("reply", {
      paramsSchema: (t) => t.Object({ id: t.Integer(), draft: t.Optional(t.Boolean()) }),
    });
    await reply({ id: "7" as unknown as number, draft: undefined }).close();
    expect(calls).toEqual([
      {
        channel: "reply",
        params: { id: 7 },
        lifecycle: {
          replayWindowMs: 600_000,
          inactivityTtlMs: 0,
          keepaliveIntervalMs: 25_000,
          maxConnectionLifetimeMs: 7_200_000,
        },
      },
    ]);
  });
});

describe("reserved param names", () => {
  test("definitions cannot declare from or last_message_id", () => {
    for (const name of ["from", "last_message_id"]) {
      expect(() =>
        defineChannel("bad", { paramsSchema: (t) => t.Object({ [name]: t.String() }) }),
      ).toThrow(`declares param '${name}', a name Tako reserves`);
    }
  });

  test("handles cannot bind from as a param", () => {
    const channel = new Channel("raw", undefined, { from: "start" });
    expect(() => channel.subscribe({ baseUrl: "https://app.example.com" })).toThrow(
      "uses a reserved name",
    );
  });
});

describe("reading from the start", () => {
  test("SSE and WebSocket URLs carry from=start unless a cursor wins", () => {
    const urls: string[] = [];
    const eventSourceFactory = (url: string) => {
      urls.push(url);
      return { close() {} };
    };
    const webSocketFactory = (url: string) => {
      urls.push(url);
      return new FakeSocket(url);
    };
    const sse = new Channel("reply", undefined, { requestId: "r1" });
    const ws = new Channel("reply", "ws", { requestId: "r1" });
    const baseUrl = "https://app.example.com";

    sse.subscribe({ baseUrl, from: "start", eventSourceFactory });
    sse.subscribe({ baseUrl, from: "start", lastEventId: "7", eventSourceFactory });
    ws.connect({ baseUrl, from: "start", webSocketFactory });
    ws.connect({ baseUrl, from: "start", lastMessageId: "7", webSocketFactory });

    expect(urls).toEqual([
      "https://app.example.com/_tako/channels/reply?requestId=r1&from=start",
      "https://app.example.com/_tako/channels/reply?requestId=r1",
      "wss://app.example.com/_tako/channels/reply?requestId=r1&from=start",
      "wss://app.example.com/_tako/channels/reply?requestId=r1&last_message_id=7",
    ]);
    expect(() => sse.subscribe({ baseUrl, from: "end" as "start", eventSourceFactory })).toThrow(
      'must be "start"',
    );
  });

  test("the fetch reader replays from the start and resumes by cursor", async () => {
    const requests: Array<{ url: string; cursor: string | null }> = [];
    configureChannels({
      fetch: mock(async (url: string, init?: RequestInit) => {
        requests.push({ url, cursor: new Headers(init?.headers).get("last-event-id") });
        return sseResponse(`id: ${requests.length}\ndata: m${requests.length}\n\n`);
      }) as unknown as typeof fetch,
    });
    const subscription = new Channel("reply").subscribe({
      baseUrl: "https://app.example.com",
      from: "start",
    });
    await (subscription.raw as SseReader).drain({ connections: 2 });
    expect(requests).toEqual([
      { url: "https://app.example.com/_tako/channels/reply?from=start", cursor: null },
      { url: "https://app.example.com/_tako/channels/reply?from=start", cursor: "1" },
    ]);
  });
});

describe("terminal signal", () => {
  test("SSE delivers earlier messages, reports the close once, and never reconnects", async () => {
    const fetchMock = mock(async () =>
      sseResponse(
        "id: 1\ndata: a\n\nid: 2\ndata: b\n\nid: 3\nevent: tako.close\ndata: {}\n\nid: 4\ndata: late\n\n",
      ),
    );
    configureChannels({ fetch: fetchMock as unknown as typeof fetch });
    const messages: string[] = [];
    const onClose = mock(() => {});
    const onRecovery = mock(() => {});
    const subscription = new Channel("reply").subscribe({
      baseUrl: "https://app.example.com",
      from: "start",
      onClose,
      onRecovery,
    });
    const reader = subscription.raw as SseReader;
    reader.addEventListener("message", (event) => messages.push((event as MessageEvent).data));
    await reader.start();
    await reader.drain();
    await new Promise((resolve) => setTimeout(resolve, 20));

    expect(messages).toEqual(["a", "b"]);
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onRecovery).not.toHaveBeenCalled();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(reader.streamClosed).toBe(true);
    expect(reader.lastEventId).toBe("3");
  });

  test("a custom EventSource is closed on tako.close so the browser does not reconnect", () => {
    const source = Object.assign(new EventTarget(), { close: mock(() => {}) });
    const onClose = mock(() => {});
    new Channel("reply").subscribe({
      baseUrl: "https://app.example.com",
      eventSourceFactory: () => source,
      onClose,
    });
    source.dispatchEvent(new MessageEvent("tako.close", { data: "{}", lastEventId: "3" }));
    source.dispatchEvent(new MessageEvent("tako.close", { data: "{}", lastEventId: "3" }));
    expect(source.close).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  test("WebSocket code 4200 reports the close once and rejects later sends", () => {
    let socket: FakeSocket | undefined;
    const onClose = mock(() => {});
    const connection = new Channel("reply", "ws").connect({
      baseUrl: "https://app.example.com",
      webSocketFactory: (url) => (socket = new FakeSocket(url)),
      onClose,
    });
    connection.send({ type: "typing" });
    socket!.dispatchEvent(new CloseEvent("close", { code: 4200, reason: "stream-closed" }));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(() => connection.send({ type: "typing" })).toThrow("stream is closed");
    expect(socket!.sent).toHaveLength(1);
  });

  test("other WebSocket closes stay reconnectable", () => {
    let socket: FakeSocket | undefined;
    const onClose = mock(() => {});
    const connection = new Channel("reply", "ws").connect({
      baseUrl: "https://app.example.com",
      webSocketFactory: (url) => (socket = new FakeSocket(url)),
      onClose,
    });
    for (const code of [1000, 1006, 4410]) {
      socket!.dispatchEvent(new CloseEvent("close", { code }));
    }
    expect(onClose).not.toHaveBeenCalled();
    expect(() => connection.send("x")).not.toThrow();
  });
});
