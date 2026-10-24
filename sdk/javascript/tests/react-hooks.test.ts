import { GlobalRegistrator } from "@happy-dom/global-registrator";

// Register before importing anything that touches React / DOM globals.
GlobalRegistrator.register();

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useChannel, usePresence } from "../src/react";
import { useLayoutEffect } from "react";
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
    expect(es!.url).toBe("http://test/_tako/channels/chat?roomId=r1&limit=10");
  });

  test("passes authorization shorthand to SSE subscriptions", () => {
    let seenHeaders: Headers | null = null;
    const authFactory = (url: string, init?: { headers?: Record<string, string> }) => {
      seenHeaders = new Headers(init?.headers);
      es = new MockEventSource(url);
      return es;
    };

    renderHook(() =>
      useChannel("chat", {
        authorization: "token-123",
        eventSourceFactory: authFactory,
        baseUrl: "http://test",
      }),
    );

    expect(seenHeaders?.get("Authorization")).toBe("Bearer token-123");
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
    expect(sockets[0]!.url).toBe("ws://test/_tako/channels/chat?roomId=r1");
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

    expect(sockets.at(-1)!.url).toBe("ws://test/_tako/channels/chat%3A1?last_message_id=m7");
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

describe("usePresence", () => {
  test("a new chat's layout update never sends to the previous chat", () => {
    const sockets: MockWebSocket[] = [];
    const factory = (url: string) => {
      const socket = new MockWebSocket(url);
      sockets.push(socket);
      return socket;
    };
    const { rerender, unmount } = renderHook(
      ({ chatId }) => {
        const value = usePresence("chat", {
          params: { chatId },
          baseUrl: "http://test",
          webSocketFactory: factory,
          data: { typing: false },
        });
        useLayoutEffect(() => {
          if (chatId === "two") value.update({ typing: true });
        }, [chatId, value.update]);
        return value;
      },
      { initialProps: { chatId: "one" } },
    );
    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(sockets[0]!.sent).toHaveLength(1);
    rerender({ chatId: "two" });
    expect(sockets[0]!.sent).toHaveLength(1);
    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(JSON.parse(sockets[1]!.sent.at(-1) as string).data).toEqual({ typing: true });
    unmount();
  });
  test("rejects invalid metadata without poisoning the next reconnect", async () => {
    const sockets: MockWebSocket[] = [];
    const { result, unmount } = renderHook(() =>
      usePresence<unknown>("lobby", {
        baseUrl: "http://test",
        data: { value: "valid" },
        webSocketFactory: (url) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
      }),
    );
    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ type: "tako.presence.ready" });
    });
    act(() => result.current.update(1n));
    expect(result.current.error).not.toBeNull();
    act(() => sockets[0]!.emitClose());
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
    });
    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(JSON.parse(sockets[1]!.sent.at(-1) as string).data).toEqual({ value: "valid" });
    unmount();
  });
  test("never renders a previous chat's roster during a scope switch", () => {
    const sockets: MockWebSocket[] = [];
    const observed: Array<{
      chatId: string;
      members: unknown[];
      status: string;
      connectionId: string | null;
    }> = [];
    const factory = (url: string) => {
      const socket = new MockWebSocket(url);
      sockets.push(socket);
      return socket;
    };
    const { rerender, unmount } = renderHook(
      ({ chatId }) => {
        const value = usePresence("chat", {
          params: { chatId },
          baseUrl: "http://test",
          webSocketFactory: factory,
          data: {},
        });
        observed.push({
          chatId,
          members: value.members,
          status: value.status,
          connectionId: value.connectionId,
        });
        return value;
      },
      { initialProps: { chatId: "one" } },
    );
    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ type: "tako.presence.ready" });
      sockets[0]!.emitMessage({
        type: "tako.presence",
        connectionId: "old",
        members: [{ connectionId: "old", data: {} }],
      });
    });
    observed.length = 0;
    rerender({ chatId: "two" });
    expect(observed.length).toBeGreaterThan(0);
    for (const value of observed) {
      expect(value.members).toEqual([]);
      expect(value.connectionId).toBeNull();
      expect(value.status).toBe("connecting");
    }
    unmount();
  });
  test("rejoins a different chat with its own initial metadata", () => {
    const sockets: MockWebSocket[] = [];
    const factory = (url: string) => {
      const socket = new MockWebSocket(url);
      sockets.push(socket);
      return socket;
    };
    const { result, rerender, unmount } = renderHook(
      ({ chatId }) =>
        usePresence("chat", {
          params: { chatId },
          baseUrl: "http://test",
          webSocketFactory: factory,
          data: { typing: false },
        }),
      { initialProps: { chatId: "one" } },
    );
    act(() => result.current.update({ typing: true }));
    rerender({ chatId: "two" });
    expect(sockets[0]!.closedWith).not.toBeNull();
    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(new URL(sockets[1]!.url).searchParams.get("chatId")).toBe("two");
    expect(JSON.parse(sockets[1]!.sent.at(-1) as string).data).toEqual({ typing: false });
    unmount();
  });

  test("stops retrying after authorization is rejected", async () => {
    const socket = new MockWebSocket("");
    let connections = 0;
    const { result, unmount } = renderHook(() =>
      usePresence("private", {
        baseUrl: "http://test",
        data: {},
        webSocketFactory: () => {
          connections++;
          return socket;
        },
      }),
    );
    act(() => socket.emitClose(4403));
    expect(result.current.error?.message).toContain("4403");
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
    });
    expect(connections).toBe(1);
    unmount();
  });
  test("joins, receives snapshots, replaces metadata, and leaves on unmount", () => {
    const socket = new MockWebSocket("");
    const { result, unmount } = renderHook(() =>
      usePresence("lobby", {
        baseUrl: "http://test",
        webSocketFactory: () => socket,
        data: { typing: false },
      }),
    );
    expect(result.current.status).toBe("connecting");
    act(() => {
      socket.emitOpen();
      socket.emitMessage({ type: "tako.presence.ready" });
    });
    expect(JSON.parse(socket.sent[0] as string)).toEqual({
      type: "tako.presence",
      data: { typing: false },
    });
    const members = [{ connectionId: "tab-1", subject: "user-1", data: { typing: false } }];
    act(() => socket.emitMessage({ type: "tako.presence", connectionId: "tab-1", members }));
    expect(result.current.members).toEqual(members);
    expect(result.current.connectionId).toBe("tab-1");
    expect(result.current.status).toBe("open");
    act(() => result.current.update({ typing: true }));
    expect(JSON.parse(socket.sent.at(-1) as string)).toEqual({
      type: "tako.presence",
      data: { typing: true },
    });
    unmount();
    expect(socket.closedWith).not.toBeNull();
  });

  test("clears stale members on disconnect and rejoins with latest data", async () => {
    const sockets: MockWebSocket[] = [];
    const { result, unmount } = renderHook(() =>
      usePresence("lobby", {
        baseUrl: "http://test",
        data: { typing: false },
        webSocketFactory: (url) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
      }),
    );
    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ type: "tako.presence.ready" });
      sockets[0]!.emitMessage({
        type: "tako.presence",
        connectionId: "old",
        members: [{ connectionId: "old", data: { typing: false } }],
      });
      result.current.update({ typing: true });
      sockets[0]!.emitClose();
    });
    expect(result.current.members).toEqual([]);
    expect(result.current.connectionId).toBeNull();
    expect(result.current.status).toBe("connecting");
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
    });
    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(JSON.parse(sockets[1]!.sent.at(-1) as string).data).toEqual({ typing: true });
    unmount();
  });
});

afterEach(() => {
  // renderHook mounts into document.body; clean up to avoid cross-test leakage.
  document.body.innerHTML = "";
});
