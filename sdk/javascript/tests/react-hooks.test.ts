import { GlobalRegistrator } from "@happy-dom/global-registrator";

// Register before importing anything that touches React / DOM globals.
GlobalRegistrator.register();

import { act, cleanup, render, renderHook, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";

// Later test files in the same run expect Bun's own globals, such as fetch.
afterAll(async () => {
  await GlobalRegistrator.unregister();
});
import { useChannel, usePresence } from "../src/react";
import { createElement, startTransition, Suspense, useLayoutEffect, useState } from "react";
import type { ChannelMessage } from "../src/types";
import { configureChannels, resetChannelsConfig } from "../src/channels/configure";

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

describe("channel recovery", () => {
  test("an ephemeral event does not add an empty cursor to WebSocket authentication", async () => {
    const sockets: MockWebSocket[] = [];
    const { unmount } = renderHook(() =>
      useChannel("chat", {
        transport: "ws",
        baseUrl: "http://test",
        authorization: "token",
        webSocketFactory: (url) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
      }),
    );
    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ id: "", eventId: { origin: "one", sequence: 1 }, data: "live" });
      sockets[0]!.emitClose();
      globalThis.dispatchEvent(new Event("online"));
    });
    await waitFor(() => expect(sockets).toHaveLength(2));
    await act(async () => sockets[1]!.emitOpen());
    expect(JSON.parse(sockets[1]!.sent[0] as string)).toEqual({
      type: "tako.auth",
      token: "Bearer token",
    });
    unmount();
  });

  test("4410 clears the URL and auth resume cursor without a business message", async () => {
    const sockets: MockWebSocket[] = [];
    const recoveries: string[] = [];
    const received: unknown[] = [];
    let resyncs = 0;
    const { result, unmount } = renderHook(() =>
      useChannel("chat", {
        transport: "ws",
        baseUrl: "http://test",
        authorization: "token",
        lastMessageId: "initial",
        webSocketFactory: (url) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
        onRecovery: (reason) => recoveries.push(reason),
        onResync: () => {
          resyncs++;
        },
        onMessage: (message) => received.push(message.data),
      }),
    );
    await act(async () => {
      sockets[0]!.emitOpen();
    });
    act(() => {
      sockets[0]!.emitMessage({ id: "expired", data: "before" });
      sockets[0]!.emitError();
      sockets[0]!.emitClose(4410);
      globalThis.dispatchEvent(new Event("online"));
    });
    await waitFor(() => expect(sockets).toHaveLength(2));
    await act(async () => {
      sockets[1]!.emitOpen();
    });
    expect(new URL(sockets[1]!.url).searchParams.has("last_message_id")).toBe(false);
    expect(JSON.parse(sockets[1]!.sent[0] as string)).toEqual({
      type: "tako.auth",
      token: "Bearer token",
    });
    act(() => {
      sockets[1]!.emitMessage({ id: "expired", data: "before" });
      sockets[1]!.emitMessage({ id: "fresh", data: "after" });
      sockets[1]!.emitClose(1000);
      globalThis.dispatchEvent(new Event("online"));
    });
    await waitFor(() => expect(sockets).toHaveLength(3));
    expect(new URL(sockets[2]!.url).searchParams.get("last_message_id")).toBe("fresh");
    expect(result.current.error).toBeNull();
    expect(result.current.status).toBe("connecting");
    expect(received).toEqual(["before", "after"]);
    expect(result.current.messages.map((message) => message.data)).toEqual(received);
    expect(recoveries).toEqual(["history-expired", "reconnecting"]);
    expect(resyncs).toBe(1);
    unmount();
  });

  test.each([4401, 4403, 4404])("stops on terminal WebSocket code %i", async (code) => {
    const sockets: MockWebSocket[] = [];
    const recoveries: string[] = [];
    const { result, unmount } = renderHook(() =>
      useChannel("private", {
        transport: "ws",
        baseUrl: "http://test",
        webSocketFactory: (url) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
        onRecovery: (reason) => recoveries.push(reason),
      }),
    );
    act(() => {
      sockets[0]!.emitError();
      sockets[0]!.emitClose(code);
      globalThis.dispatchEvent(new Event("online"));
    });
    expect(result.current.error?.message).toContain(String(code));
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1400));
    });
    expect(sockets).toHaveLength(1);
    expect(recoveries).toEqual([]);
    unmount();
  });

  test.each(["name", "params", "baseUrl", "authorization", "headers"])(
    "fences messages, callbacks, sends, and cursors when %s changes",
    (field) => {
      const sockets: MockWebSocket[] = [];
      const received: string[] = [];
      const observed: unknown[][] = [];
      const { result, rerender, unmount } = renderHook(
        ({ changed }) => {
          const value = useChannel(changed && field === "name" ? "two" : "one", {
            transport: "ws",
            baseUrl: changed && field === "baseUrl" ? "http://other" : "http://test",
            params: { room: changed && field === "params" ? "two" : "one" },
            authorization: changed && field === "authorization" ? "two" : "one",
            headers: { "X-Tenant": changed && field === "headers" ? "two" : "one" },
            webSocketFactory: (url) => {
              const socket = new MockWebSocket(url);
              sockets.push(socket);
              return socket;
            },
            onMessage: () => received.push(changed ? "new" : "old"),
          });
          if (changed) observed.push(value.messages);
          useLayoutEffect(() => {
            if (!changed) return;
            sockets[0]!.emitMessage({ id: "late", data: "old late response" });
            sockets[0]!.emitError();
            value.send("new identity payload");
          }, [changed]);
          return value;
        },
        { initialProps: { changed: false } },
      );
      act(() => {
        sockets[0]!.emitOpen();
        sockets[0]!.emitMessage({ id: "shared", data: "old" });
      });
      const oldConnection = result.current;
      const oldClear = () => oldConnection.clear();
      const oldSend = result.current.send;
      rerender({ changed: true });
      expect(sockets).toHaveLength(2);
      expect(sockets[0]!.sent).toEqual([]);
      expect(observed.every((messages) => messages.length === 0)).toBe(true);
      expect(new URL(sockets[1]!.url).searchParams.has("last_message_id")).toBe(false);
      act(() => {
        sockets[0]!.emitMessage({ id: "late", data: "old" });
        sockets[1]!.emitMessage({ id: "shared", data: "new" });
        oldClear();
        oldSend("old callback");
      });
      expect(received).toEqual(["old", "new"]);
      expect(result.current.messages.map((message) => message.data)).toEqual(["new"]);
      expect(result.current.error).toBeNull();
      expect(sockets[1]!.sent).toEqual([]);
      unmount();
    },
  );

  test("bounds replay deduplication to the most recent 500 IDs", () => {
    const source = new MockEventSource("");
    const received: unknown[] = [];
    const { result, unmount } = renderHook(() =>
      useChannel("chat", {
        baseUrl: "http://test",
        eventSourceFactory: () => source,
        onMessage: (message) => received.push(message.data),
      }),
    );
    act(() => {
      for (let index = 0; index <= 500; index++)
        source.emitMessage({ id: String(index), data: index });
      source.emitMessage({ id: "500", data: "duplicate" });
      source.emitMessage({ id: "0", data: "evicted" });
    });
    expect(received).toHaveLength(502);
    expect(result.current.messages).toHaveLength(500);
    expect(result.current.messages.at(-1)?.data).toBe("evicted");
    unmount();
  });

  test("SSE recovery marks connecting and resyncs without fake payloads or routine errors", async () => {
    const recoveries: string[] = [];
    let resyncs = 0;
    let calls = 0;
    configureChannels({
      fetch: (async () => {
        calls++;
        return new Response(null, { status: 410 });
      }) as typeof fetch,
    });
    const { result, unmount } = renderHook(() =>
      useChannel("chat", {
        baseUrl: "http://test",
        onRecovery: (reason) => recoveries.push(reason),
        onResync: () => {
          resyncs++;
        },
      }),
    );
    await waitFor(() => expect(recoveries).toEqual(["history-expired"]));
    expect(calls).toBe(1);
    expect(resyncs).toBe(1);
    expect(result.current).toMatchObject({ status: "connecting", error: null, messages: [] });
    unmount();
    resetChannelsConfig();
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

  test("switching accounts resets metadata even in the same room", () => {
    const sockets: MockWebSocket[] = [];
    const { result, rerender, unmount } = renderHook(
      ({ account }) =>
        usePresence("lobby", {
          baseUrl: "http://test",
          authorization: account,
          data: { document: account },
          webSocketFactory: (url) => {
            const socket = new MockWebSocket(url);
            sockets.push(socket);
            return socket;
          },
        }),
      { initialProps: { account: "one" } },
    );
    act(() => result.current.update({ document: "private-to-one" }));
    rerender({ account: "two" });
    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(JSON.parse(sockets[1]!.sent.at(-1) as string).data).toEqual({ document: "two" });
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
  test("rejoins after a 4410 reset instead of treating it as a rejection", async () => {
    const sockets: MockWebSocket[] = [];
    const { result, unmount } = renderHook(() =>
      usePresence("lobby", {
        baseUrl: "http://test",
        data: { typing: false },
        webSocketFactory: (url: string) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
      }),
    );
    const members = [{ connectionId: "tab-1", subject: "user-1", data: { typing: false } }];
    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ type: "tako.presence.ready" });
      sockets[0]!.emitMessage({ type: "tako.presence", connectionId: "tab-1", members });
    });
    expect(result.current.status).toBe("open");
    act(() => sockets[0]!.emitClose(4410));
    expect(result.current.status).toBe("connecting");
    expect(result.current.error).toBeNull();
    expect(result.current.members).toEqual([]);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
    });
    expect(sockets).toHaveLength(2);
    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ type: "tako.presence.ready" });
    });
    expect(JSON.parse(sockets[1]!.sent[0] as string)).toEqual({
      type: "tako.presence",
      data: { typing: false },
    });
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
  cleanup();
  resetChannelsConfig();
});

test.each([true, false])(
  "suspended render preserves committed messages and callbacks (identity change: %s)",
  async (changeIdentity) => {
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    let calls = 0,
      closed = 0,
      suspended = 0;
    configureChannels({
      fetch: (async (_input, init) => {
        calls++;
        return new Response(
          new ReadableStream<Uint8Array>({
            start(c) {
              controller = c;
              init?.signal?.addEventListener(
                "abort",
                () => {
                  closed++;
                  try {
                    c.close();
                  } catch {}
                },
                { once: true },
              );
            },
          }),
          { headers: { "content-type": "text/event-stream" } },
        );
      }) as typeof fetch,
    });
    let setRoom!: (room: string) => void;
    const never = new Promise(() => {});
    const messages: string[] = [];
    function Page() {
      const [room, set] = useState("old");
      setRoom = set;
      const channel = useChannel(changeIdentity ? room : "same", {
        baseUrl: "https://offline.test",
        onMessage: () => messages.push(room),
      });
      if (room === "pending") {
        suspended++;
        throw never;
      }
      return createElement("div", null, `${room}:${channel.status}:${channel.messages.length}`);
    }
    const page = render(
      createElement(
        Suspense,
        { fallback: createElement("div", null, "fallback") },
        createElement(Page),
      ),
    );
    await act(async () => {
      await new Promise((r) => setTimeout(r, 10));
    });
    await act(async () => {
      controller.enqueue(new TextEncoder().encode('id: 1\ndata: {"id":"1","data":"first"}\n\n'));
    });
    expect(page.container.textContent).toBe("old:open:1");
    await act(async () => {
      startTransition(() => setRoom("pending"));
    });
    expect(suspended).toBeGreaterThan(0);
    expect(page.container.textContent).toBe("old:open:1");
    await act(async () => {
      controller.enqueue(new TextEncoder().encode('id: 2\ndata: {"id":"2","data":"during"}\n\n'));
    });
    // Cancel the suspended transition and keep the original identity visible.
    await act(async () => {
      setRoom("old");
    });
    await act(async () => {
      controller.enqueue(new TextEncoder().encode('id: 3\ndata: {"id":"3","data":"after"}\n\n'));
    });
    expect(calls).toBe(1);
    expect(closed).toBe(0);
    expect(messages).toEqual(["old", "old", "old"]);
    expect(page.container.textContent).toBe("old:open:3");
  },
  10000,
);

test("publishes identity and callbacks before descendant layout actions", () => {
  const sockets: MockWebSocket[] = [];
  const received: string[] = [];
  let oldSend!: (data: unknown) => void;
  function Consumer({ changed }: { changed: boolean }) {
    useLayoutEffect(() => {
      if (!changed) return;
      oldSend("late send");
      sockets[0]!.emitMessage({ id: "late", data: "old" });
    }, [changed]);
    return null;
  }
  function Page({ changed }: { changed: boolean }) {
    const channel = useChannel(changed ? "new" : "old", {
      transport: "ws",
      baseUrl: "http://test",
      webSocketFactory: (url) => {
        const socket = new MockWebSocket(url);
        sockets.push(socket);
        return socket;
      },
      onMessage: () => received.push(changed ? "new" : "old"),
    });
    if (!changed) oldSend = channel.send;
    return createElement(Consumer, { changed });
  }
  const page = render(createElement(Page, { changed: false }));
  act(() => sockets[0]!.emitOpen());
  page.rerender(createElement(Page, { changed: true }));
  expect(sockets[0]!.sent).toEqual([]);
  expect(received).toEqual([]);
  expect(sockets).toHaveLength(2);
});

test.each([
  ["sse", "onRecovery", "sync"],
  ["sse", "onRecovery", "async"],
  ["sse", "onResync", "sync"],
  ["sse", "onResync", "async"],
  ["sse", "both", "sync"],
  ["sse", "both", "async"],
  ["ws", "onRecovery", "sync"],
  ["ws", "onRecovery", "async"],
  ["ws", "onResync", "sync"],
  ["ws", "onResync", "async"],
  ["ws", "both", "sync"],
  ["ws", "both", "async"],
] as const)(
  "failing %s recovery observers (%s, %s) do not prevent reconnect",
  async (transport, throwing, mode) => {
    const diagnostic = spyOn(console, "error").mockImplementation(() => {});
    const observerError = new Error("observer failed");
    const observed: string[] = [];
    const rejections: Array<(error: Error) => void> = [];
    const fail = () => {
      if (mode === "async") return new Promise<void>((_, reject) => rejections.push(reject));
      throw observerError;
    };
    const sockets: MockWebSocket[] = [];
    let fetches = 0;
    configureChannels({
      fetch: (async (_input, init) => {
        if (++fetches === 1) return new Response(null, { status: 410 });
        return new Response(
          new ReadableStream({
            start(controller) {
              init?.signal?.addEventListener("abort", () => controller.close(), { once: true });
            },
          }),
          { headers: { "content-type": "text/event-stream" } },
        );
      }) as typeof fetch,
    });
    const observers = {
      onRecovery: () => {
        observed.push("recovery");
        if (throwing !== "onResync") return fail();
      },
      onResync: () => {
        observed.push("resync");
        if (throwing !== "onRecovery") return fail();
      },
    };
    const { result, unmount } = renderHook(() =>
      transport === "sse"
        ? useChannel("chat", { baseUrl: "http://test", ...observers })
        : useChannel("chat", {
            transport: "ws",
            baseUrl: "http://test",
            ...observers,
            webSocketFactory: (url) => {
              const socket = new MockWebSocket(url);
              sockets.push(socket);
              return socket;
            },
          }),
    );
    try {
      if (transport === "ws") {
        act(() => {
          sockets[0]!.emitClose(4410);
          globalThis.dispatchEvent(new Event("online"));
        });
        await waitFor(() => expect(sockets).toHaveLength(2));
        act(() => sockets[1]!.emitOpen());
      } else {
        await waitFor(() => expect(fetches).toBe(2), { timeout: 2000 });
      }
      expect(observed).toEqual(["recovery", "resync"]);
      await act(async () => {
        for (const reject of rejections) reject(observerError);
      });
      expect(diagnostic).toHaveBeenCalledTimes(throwing === "both" ? 2 : 1);
      if (throwing !== "onResync")
        expect(diagnostic).toHaveBeenCalledWith(
          "useChannel onRecovery handler threw:",
          observerError,
        );
      if (throwing !== "onRecovery")
        expect(diagnostic).toHaveBeenCalledWith(
          "useChannel onResync handler threw:",
          observerError,
        );
      expect(result.current.status).toBe("open");
      expect(result.current.error).toBeNull();
      expect(result.current.messages).toEqual([]);
    } finally {
      unmount();
      diagnostic.mockRestore();
    }
  },
);

describe("useChannel closed streams", () => {
  test("SSE from the start replays, reports closed once, and never reconnects", () => {
    const sources: MockEventSource[] = [];
    const onClose: string[] = [];
    const received: unknown[] = [];
    const { result } = renderHook(() =>
      useChannel<string>("reply", {
        params: { requestId: "r1" },
        from: "start",
        baseUrl: "http://test",
        eventSourceFactory: (url: string) => {
          const source = new MockEventSource(url);
          sources.push(source);
          return source;
        },
        onMessage: (msg) => received.push(msg.data),
        onClose: () => onClose.push("closed"),
      }),
    );
    expect(sources[0]!.url).toBe("http://test/_tako/channels/reply?requestId=r1&from=start");

    act(() => {
      sources[0]!.emitOpen();
      sources[0]!.emitMessage({ id: "1", channel: "reply", type: "chunk", data: "Hel" });
      sources[0]!.emitMessage({ id: "2", channel: "reply", type: "chunk", data: "lo" });
      sources[0]!.dispatchEvent(new MessageEvent("tako.close", { data: "{}", lastEventId: "3" }));
    });

    expect(result.current.messages.map((msg) => msg.data)).toEqual(["Hel", "lo"]);
    expect(received).toEqual(["Hel", "lo"]);
    expect(result.current.closed).toBe(true);
    expect(result.current.status).toBe("closed");
    expect(result.current.error).toBeNull();
    expect(sources[0]!.closed).toBe(true);
    expect(onClose).toEqual(["closed"]);
    expect(sources).toHaveLength(1);

    act(() => {
      sources[0]!.emitError();
    });
    expect(result.current.status).toBe("closed");
  });

  test("WebSocket from the start resumes by cursor, then stops at 4200", async () => {
    const sockets: MockWebSocket[] = [];
    let closes = 0;
    const { result } = renderHook(() =>
      useChannel<string>("reply", {
        transport: "ws",
        params: { requestId: "r1" },
        from: "start",
        baseUrl: "http://test",
        webSocketFactory: (url: string) => {
          const socket = new MockWebSocket(url);
          sockets.push(socket);
          return socket;
        },
        onClose: () => closes++,
      }),
    );
    expect(sockets[0]!.url).toBe("ws://test/_tako/channels/reply?requestId=r1&from=start");

    act(() => {
      sockets[0]!.emitOpen();
      sockets[0]!.emitMessage({ id: "m1", channel: "reply", type: "chunk", data: "Hel" });
      sockets[0]!.emitClose(1006);
    });
    expect(result.current.closed).toBe(false);
    await act(async () => {
      await new Promise((r) => setTimeout(r, 1500));
    });
    expect(sockets).toHaveLength(2);
    expect(sockets[1]!.url).toBe("ws://test/_tako/channels/reply?requestId=r1&last_message_id=m1");

    act(() => {
      sockets[1]!.emitOpen();
      sockets[1]!.emitMessage({ id: "m2", channel: "reply", type: "chunk", data: "lo" });
      sockets[1]!.emitClose(4200);
    });
    expect(result.current.messages.map((msg) => msg.data)).toEqual(["Hel", "lo"]);
    expect(result.current.status).toBe("closed");
    expect(result.current.closed).toBe(true);
    expect(closes).toBe(1);
    expect(() => result.current.send?.({ type: "late" })).toThrow("stream is closed");

    await act(async () => {
      await new Promise((r) => setTimeout(r, 1500));
    });
    expect(sockets).toHaveLength(2);
  });
});
