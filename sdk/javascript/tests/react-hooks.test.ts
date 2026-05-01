import { GlobalRegistrator } from "@happy-dom/global-registrator";

// Register before importing anything that touches React / DOM globals.
GlobalRegistrator.register();

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useChannel } from "../src/react";
import type { ChannelMessage } from "../src/types";

class MockEventSource extends EventTarget {
  url: string;
  closed = false;
  readyState = 0;
  constructor(url: string) {
    super();
    this.url = url;
  }
  close() {
    this.closed = true;
  }
  emitOpen() {
    this.readyState = 1;
    this.dispatchEvent(new Event("open"));
  }
  emitMessage(data: unknown) {
    this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify(data) }));
  }
  emitError() {
    this.dispatchEvent(new Event("error"));
  }
}

class MockWebSocket extends EventTarget {
  url: string;
  readyState = 0;
  sent: unknown[] = [];
  closedWith: { code?: number; reason?: string } | null = null;
  constructor(url: string) {
    super();
    this.url = url;
  }
  send(data: unknown) {
    this.sent.push(data);
  }
  close(code?: number, reason?: string) {
    this.closedWith = { code, reason };
    this.readyState = 3;
    this.dispatchEvent(new CloseEvent("close", { code, reason }));
  }
  emitOpen() {
    this.readyState = 1;
    this.dispatchEvent(new Event("open"));
  }
  emitMessage(data: unknown) {
    this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify(data) }));
  }
  emitError() {
    this.dispatchEvent(new Event("error"));
  }
  emitClose(code = 1006) {
    this.readyState = 3;
    this.dispatchEvent(new CloseEvent("close", { code }));
  }
}

describe("useChannel (sse)", () => {
  let es: MockEventSource | null = null;
  const factory = (url: string) => {
    es = new MockEventSource(url);
    return es;
  };

  beforeEach(() => {
    es = null;
  });

  test("starts in connecting state and transitions to open", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", { eventSourceFactory: factory, baseUrl: "http://test" }),
    );
    expect(result.current.status).toBe("connecting");
    expect(result.current.messages).toEqual([]);
    expect(es).not.toBeNull();

    act(() => {
      es!.emitOpen();
    });
    expect(result.current.status).toBe("open");
  });

  test("passes params as query string to SSE subscriptions", () => {
    renderHook(() =>
      useChannel("chat", {
        params: { roomId: "r1", limit: 10 },
        eventSourceFactory: factory,
        baseUrl: "http://test",
      }),
    );
    expect(es!.url).toBe("http://test/channels/chat?roomId=r1&limit=10");
  });

  test("appends messages on incoming events", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", { eventSourceFactory: factory, baseUrl: "http://test" }),
    );
    act(() => {
      es!.emitOpen();
      es!.emitMessage({ id: "m1", channel: "chat:1", type: "chat", data: { body: "hi" } });
    });
    expect(result.current.messages).toHaveLength(1);
    expect(result.current.messages[0]).toEqual({
      id: "m1",
      channel: "chat:1",
      type: "chat",
      data: { body: "hi" },
    });
  });

  test("caps message history at 500", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", { eventSourceFactory: factory, baseUrl: "http://test" }),
    );
    act(() => {
      es!.emitOpen();
      for (let i = 0; i < 600; i++) {
        es!.emitMessage({ id: `m${i}`, channel: "chat:1", type: "chat", data: i });
      }
    });
    expect(result.current.messages).toHaveLength(500);
    expect(result.current.messages[0]?.data).toBe(100);
    expect(result.current.messages.at(-1)?.data).toBe(599);
  });

  test("flips back to connecting on error and surfaces it", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", { eventSourceFactory: factory, baseUrl: "http://test" }),
    );
    act(() => {
      es!.emitOpen();
    });
    expect(result.current.status).toBe("open");
    act(() => {
      es!.emitError();
    });
    expect(result.current.status).toBe("connecting");
    expect(result.current.error).not.toBeNull();
  });

  test("clear() empties the message buffer", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", { eventSourceFactory: factory, baseUrl: "http://test" }),
    );
    act(() => {
      es!.emitOpen();
      es!.emitMessage({ id: "m1", channel: "chat:1", type: "chat", data: 1 });
    });
    expect(result.current.messages).toHaveLength(1);
    act(() => {
      result.current.clear();
    });
    expect(result.current.messages).toHaveLength(0);
  });

  test("closes the event source on unmount", () => {
    const { unmount } = renderHook(() =>
      useChannel("chat:1", { eventSourceFactory: factory, baseUrl: "http://test" }),
    );
    expect(es!.closed).toBe(false);
    unmount();
    expect(es!.closed).toBe(true);
  });

  test("onMessage option fires for each incoming message", () => {
    const received: unknown[] = [];
    renderHook(() =>
      useChannel<string>("chat:1", {
        eventSourceFactory: factory,
        baseUrl: "http://test",
        onMessage: (msg) => received.push(msg.data),
      }),
    );

    act(() => {
      es!.emitOpen();
      es!.emitMessage({ id: "m1", channel: "chat:1", type: "chat", data: "a" });
      es!.emitMessage({ id: "m2", channel: "chat:1", type: "chat", data: "b" });
    });
    expect(received).toEqual(["a", "b"]);
  });

  test("onMessage uses the latest handler without reconnecting", () => {
    const calls: string[] = [];
    const { rerender } = renderHook(
      ({ handler }: { handler: (msg: ChannelMessage<string>) => void }) =>
        useChannel<string>("chat:1", {
          eventSourceFactory: factory,
          baseUrl: "http://test",
          onMessage: handler,
        }),
      {
        initialProps: {
          handler: (_msg: ChannelMessage<string>) => calls.push("first"),
        },
      },
    );

    const firstSource = es;
    rerender({
      handler: (_msg: ChannelMessage<string>) => calls.push("second"),
    });
    // Same EventSource — swapping the handler must not reconnect.
    expect(es).toBe(firstSource);

    act(() => {
      es!.emitOpen();
      es!.emitMessage({ id: "m1", channel: "chat:1", type: "chat", data: "x" });
    });
    expect(calls).toEqual(["second"]);
  });
});

describe("useChannel (ws)", () => {
  let sockets: MockWebSocket[] = [];
  const factory = (url: string) => {
    const ws = new MockWebSocket(url);
    sockets.push(ws);
    return ws;
  };

  beforeEach(() => {
    sockets = [];
  });

  test("starts connecting, opens, receives messages", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", {
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
      }),
    );
    expect(result.current.status).toBe("connecting");
    expect(sockets).toHaveLength(1);

    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ id: "m1", channel: "chat:1", type: "chat", data: "hi" });
    });
    expect(result.current.status).toBe("open");
    expect(result.current.messages).toHaveLength(1);
  });

  test("passes params as query string to websocket connections", () => {
    renderHook(() =>
      useChannel("chat", {
        params: { roomId: "r1" },
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
      }),
    );
    expect(sockets[0]!.url).toBe("ws://test/channels/chat?roomId=r1");
  });

  test("send() forwards to the underlying socket", () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", {
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
      }),
    );
    act(() => {
      sockets[0]!.emitOpen();
    });
    act(() => {
      result.current.send?.({ type: "chat", body: "hello" });
    });
    expect(sockets[0]!.sent).toHaveLength(1);
  });

  test("auto-reconnects after an unexpected close", async () => {
    const { result } = renderHook(() =>
      useChannel("chat:1", {
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
      }),
    );
    act(() => {
      sockets[0]!.emitOpen();
    });
    expect(result.current.status).toBe("open");

    act(() => {
      sockets[0]!.emitClose();
    });
    expect(result.current.status).toBe("connecting");

    // Backoff min is 1s with jitter — wait a bit past that for the retry socket.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 1500));
    });
    expect(sockets.length).toBeGreaterThanOrEqual(2);

    act(() => {
      sockets.at(-1)!.emitOpen();
    });
    expect(result.current.status).toBe("open");
  });

  test("reconnect resumes from the last received message id", async () => {
    renderHook(() =>
      useChannel("chat:1", {
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
      }),
    );

    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ id: "m7", channel: "chat:1", type: "chat", data: "before" });
      sockets[0]!.emitClose();
    });

    await act(async () => {
      await new Promise((r) => setTimeout(r, 1500));
    });

    expect(sockets.at(-1)!.url).toBe("ws://test/channels/chat%3A1?last_message_id=m7");
  });

  test("closes the socket on unmount and stops reconnecting", async () => {
    const { unmount } = renderHook(() =>
      useChannel("chat:1", {
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
      }),
    );
    unmount();
    expect(sockets[0]!.closedWith).not.toBeNull();

    await new Promise((r) => setTimeout(r, 1500));
    expect(sockets).toHaveLength(1);
  });

  test("onMessage option fires over the websocket and survives reconnect", async () => {
    const received: unknown[] = [];
    renderHook(() =>
      useChannel<number>("chat:1", {
        transport: "ws",
        webSocketFactory: factory,
        baseUrl: "http://test",
        onMessage: (msg) => received.push(msg.data),
      }),
    );

    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ id: "m1", channel: "chat:1", type: "chat", data: 1 });
      sockets[0]!.emitClose();
    });

    await act(async () => {
      await new Promise((r) => setTimeout(r, 1500));
    });
    act(() => {
      sockets.at(-1)!.emitOpen();
      sockets.at(-1)!.emitMessage({ id: "m2", channel: "chat:1", type: "chat", data: 2 });
    });

    expect(received).toEqual([1, 2]);
  });
});

afterEach(() => {
  // renderHook mounts into document.body; clean up to avoid cross-test leakage.
  document.body.innerHTML = "";
});
