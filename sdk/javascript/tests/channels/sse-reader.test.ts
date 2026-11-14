import { describe, expect, spyOn, test } from "bun:test";
import { Channel, type ChannelSubscribeOptions } from "../../src/channels";
import { configureChannels, resetChannelsConfig } from "../../src/channels/configure";
import { SseReader, type SseRecoveryReason } from "../../src/channels/sse-reader";

function mockFetchStream(chunks: string[]): typeof fetch {
  return async () => {
    const encoder = new TextEncoder();
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        for (const chunk of chunks) {
          controller.enqueue(encoder.encode(chunk));
        }
        controller.close();
      },
    });
    return new Response(stream, {
      status: 200,
      headers: { "content-type": "text/event-stream" },
    });
  };
}

describe("SseReader", () => {
  test("recovers a midstream reset without delivering a control message", async () => {
    const messages: string[] = [];
    const recovery: SseRecoveryReason[] = [];
    const cursors: (string | null)[] = [];
    let requests = 0;
    const reader = new SseReader("http://fixture/events", {
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      jitter: 0,
      fetch: Object.assign(
        async (_input: RequestInfo | URL, init?: RequestInit) => {
          cursors.push(new Headers(init?.headers).get("last-event-id"));
          requests++;
          return new Response(
            requests === 1
              ? "id: first\ndata: first\n\nid:\nevent: tako.reset\ndata: {}\n\nid: invalid\ndata: stale\n\n"
              : "id: second\ndata: second\n\n",
            { headers: { "content-type": "text/event-stream" } },
          );
        },
        { preconnect: fetch.preconnect },
      ),
      onRecovery: (reason) => recovery.push(reason),
      onMessage: (message) => {
        messages.push(message.data);
        if (message.data === "second") reader.close();
      },
    });
    await reader.start();
    await reader.drain();
    expect(messages).toEqual(["first", "second"]);
    expect(recovery).toEqual(["history-expired"]);
    expect(cursors).toEqual([null, null]);
    expect(reader.lastEventId).toBe("second");
  });

  test("dispatches messages assembled from data lines", async () => {
    const messages: string[] = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: mockFetchStream(["data: hello\n\n", "data: world\n\n"]),
      headers: { Authorization: "Bearer t" },
      onMessage: (message) => messages.push(message.data),
    });

    await reader.start();
    await reader.drain();

    expect(messages).toEqual(["hello", "world"]);
  });

  test("joins multiple data lines with newlines", async () => {
    const messages: string[] = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: mockFetchStream(["event: note\ndata: hello\ndata: world\n\n"]),
      onMessage: (message) => messages.push(`${message.type}:${message.data}`),
    });

    await reader.start();
    await reader.drain();

    expect(messages).toEqual(["note:hello\nworld"]);
  });

  test("tracks lastEventId from id lines", async () => {
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: mockFetchStream(["id: 7\ndata: a\n\n"]),
      onMessage: () => {},
    });

    await reader.start();
    await reader.drain();

    expect(reader.lastEventId).toBe("7");
  });

  test("does not retry fetch failures by default", async () => {
    let calls = 0;
    const errors: string[] = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: async () => {
        calls++;
        throw new Error("net");
      },
      onMessage: () => {},
      onError: (error) => errors.push(error.message),
      backoffBaseMs: 1,
      backoffMaxMs: 5,
      jitter: 0,
    });

    await reader.start();
    await reader.drain();

    expect(calls).toBe(1);
    expect(errors).toEqual(["net"]);
  });

  test("retries with backoff after fetch failure when retryOnDisconnect is enabled", async () => {
    let calls = 0;
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: async () => {
        calls++;
        if (calls === 1) {
          throw new Error("net");
        }
        return new Response("data: ok\n\n", {
          headers: { "content-type": "text/event-stream" },
        });
      },
      onMessage: () => {},
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      backoffMaxMs: 5,
      jitter: 0,
    });

    await reader.start();
    await reader.drain({ connections: 1 });

    expect(calls).toBe(2);
  });

  test("sends Last-Event-ID on retry", async () => {
    const seen: Array<string | null> = [];
    let calls = 0;
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: async (_url, init) => {
        calls++;
        seen.push(new Headers(init?.headers).get("Last-Event-ID"));
        if (calls === 1) {
          return new Response("id: 7\ndata: a\n\n", {
            headers: { "content-type": "text/event-stream" },
          });
        }
        return new Response("data: b\n\n", {
          headers: { "content-type": "text/event-stream" },
        });
      },
      onMessage: () => {},
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      backoffMaxMs: 1,
      jitter: 0,
    });

    await reader.start();
    await reader.drain({ connections: 2 });

    expect(seen).toEqual([null, "7"]);
  });

  test("treats a clean stream end as reconnectable when retryOnDisconnect is enabled", async () => {
    const errors: string[] = [];
    const recoveries: SseRecoveryReason[] = [];
    let calls = 0;
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: async () => {
        calls++;
        return new Response("data: ok\n\n", {
          headers: { "content-type": "text/event-stream" },
        });
      },
      onMessage: () => {},
      onError: (error) => errors.push(error.message),
      onRecovery: (reason) => recoveries.push(reason),
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      backoffMaxMs: 1,
      jitter: 0,
    });

    await reader.start();
    await reader.drain({ connections: 2 });

    expect(calls).toBe(2);
    expect(errors).toEqual([]);
    expect(recoveries).toEqual(["reconnecting"]);
  });

  test.each(["Last-Event-ID", "last-event-id", "LAST-EVENT-ID"])(
    "clears outgoing %s after 410 and resumes from new messages",
    async (header) => {
      const seen: Array<string | null> = [];
      const authorizations: Array<string | null> = [];
      const messages: string[] = [];
      const events: string[] = [];
      const errors: Error[] = [];
      const recoveries: SseRecoveryReason[] = [];
      const reader = new SseReader("http://x/_tako/channels/sse/chat", {
        headers: { [header]: "initial", Authorization: "Bearer t" },
        fetch: async (_url, init) => {
          const headers = new Headers(init?.headers);
          seen.push(headers.get("last-event-id"));
          authorizations.push(headers.get("authorization"));
          switch (seen.length) {
            case 1:
              return new Response("id: expired\ndata: before\n\n");
            case 2:
              return new Response(null, { status: 410 });
            case 3:
              throw new Error("offline");
            case 4:
              return new Response("id: fresh\ndata: after\n\n");
            default:
              return new Response(null, { status: 403 });
          }
        },
        onMessage: (message) => messages.push(message.data),
        onRecovery: (reason) => {
          recoveries.push(reason);
          if (reason === "history-expired") expect(reader.lastEventId).toBeUndefined();
        },
        onError: (error) => errors.push(error),
        retryOnDisconnect: true,
        backoffBaseMs: 1,
        backoffMaxMs: 1,
        jitter: 0,
      });
      reader.addEventListener("message", (event) => events.push((event as MessageEvent).data));

      await reader.start();
      await reader.drain();

      expect(seen).toEqual(["initial", "expired", null, null, "fresh"]);
      expect(authorizations).toEqual(Array(5).fill("Bearer t"));
      expect(messages).toEqual(["before", "after"]);
      expect(events).toEqual(messages);
      expect(recoveries).toEqual([
        "reconnecting",
        "history-expired",
        "reconnecting",
        "reconnecting",
      ]);
      expect(errors.map((error) => error.message)).toEqual(["SSE request failed with status 403."]);
      expect(reader.lastEventId).toBe("fresh");
    },
  );

  test("clears an explicit initial cursor when the first request returns 410", async () => {
    const seen: Array<string | null> = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      headers: { "Last-Event-ID": "header-cursor" },
      fetch: async (_url, init) => {
        seen.push(new Headers(init?.headers).get("last-event-id"));
        return seen.length === 1
          ? new Response(null, { status: 410 })
          : new Response("data: live\n\n");
      },
      onMessage: () => {},
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      jitter: 0,
    });
    reader.lastEventId = "explicit-cursor";

    await reader.start();
    await reader.drain({ connections: 1 });

    expect(seen).toEqual(["explicit-cursor", null]);
    expect(reader.lastEventId).toBeUndefined();
  });

  test.each([401, 403, 404])("stops on HTTP %i without recovery", async (status) => {
    let calls = 0;
    const errors: Error[] = [];
    const events: Error[] = [];
    const recoveries: SseRecoveryReason[] = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: async () => {
        calls++;
        return new Response(null, { status });
      },
      onMessage: () => {},
      onError: (error) => errors.push(error),
      onRecovery: (reason) => recoveries.push(reason),
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      jitter: 0,
    });
    reader.addEventListener("error", (event) => events.push((event as ErrorEvent).error));

    await reader.start();
    await reader.drain({ connections: 1 });
    reader.close();

    expect(calls).toBe(1);
    expect(errors.map((error) => error.message)).toEqual([
      `SSE request failed with status ${status}.`,
    ]);
    expect(events).toEqual(errors);
    expect(recoveries).toEqual([]);
  });

  test("retries temporary HTTP, fetch, and stream failures without error events", async () => {
    let calls = 0;
    const errors: Error[] = [];
    const recoveries: SseRecoveryReason[] = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      fetch: async () => {
        switch (++calls) {
          case 1:
            return new Response(null, { status: 503 });
          case 2:
            throw new Error("offline");
          case 3:
            return new Response(
              new ReadableStream({
                start(controller) {
                  controller.error(new Error("broken stream"));
                },
              }),
            );
          default:
            return new Response("data: recovered\n\n");
        }
      },
      onMessage: () => {},
      onError: (error) => errors.push(error),
      onRecovery: (reason) => recoveries.push(reason),
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      backoffMaxMs: 2,
      jitter: 0,
    });
    reader.addEventListener("error", (event) => errors.push((event as ErrorEvent).error));

    await reader.start();
    await reader.drain({ connections: 2 });

    expect(calls).toBe(4);
    expect(errors).toEqual([]);
    expect(recoveries).toEqual(Array(3).fill("reconnecting"));
  });

  test("does not fetch with an already aborted signal", async () => {
    let calls = 0;
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      signal: AbortSignal.abort(),
      fetch: async () => {
        calls++;
        return new Response();
      },
      onMessage: () => {},
    });

    await reader.start();
    await reader.drain({ connections: 1 });

    expect(calls).toBe(0);
  });

  test("aborts while waiting for a retry", async () => {
    const controller = new AbortController();
    let calls = 0;
    const errors: Error[] = [];
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      signal: controller.signal,
      fetch: async () => {
        calls++;
        throw new Error("offline");
      },
      onMessage: () => {},
      onError: (error) => errors.push(error),
      onRecovery: () => {
        queueMicrotask(() => controller.abort());
      },
      retryOnDisconnect: true,
      backoffBaseMs: 60_000,
      jitter: 0,
    });

    await reader.start();
    await reader.drain();

    expect(calls).toBe(1);
    expect(errors).toEqual([]);
  });

  test("passes abort to an in-flight custom fetch", async () => {
    const controller = new AbortController();
    const errors: Error[] = [];
    const recoveries: SseRecoveryReason[] = [];
    let fetchSignal: AbortSignal | null | undefined;
    const reader = new SseReader("http://x/_tako/channels/sse/chat", {
      signal: controller.signal,
      fetch: async (_url, init) => {
        fetchSignal = init?.signal;
        return new Promise<Response>((_resolve, reject) => {
          fetchSignal?.addEventListener("abort", () => reject(new Error("aborted")), {
            once: true,
          });
        });
      },
      onMessage: () => {},
      onError: (error) => errors.push(error),
      onRecovery: (reason) => recoveries.push(reason),
      retryOnDisconnect: true,
    });

    const started = reader.start();
    controller.abort();
    await started;
    await reader.drain();

    expect(fetchSignal?.aborted).toBe(true);
    expect(errors).toEqual([]);
    expect(recoveries).toEqual([]);
  });

  test("Channel.subscribe forwards recovery and terminal errors using configured fetch", async () => {
    const recoveries: SseRecoveryReason[] = [];
    const errors: Error[] = [];
    let calls = 0;
    configureChannels({
      fetch: async () => {
        calls++;
        return new Response(null, { status: calls === 1 ? 410 : 401 });
      },
    });
    try {
      const options: ChannelSubscribeOptions = {
        baseUrl: "http://x",
        onRecovery: (reason) => recoveries.push(reason),
        onError: (error) => errors.push(error),
      };
      const subscription = new Channel("chat").subscribe(options);
      await (subscription.raw as SseReader).drain();

      expect(calls).toBe(2);
      expect(recoveries).toEqual(["history-expired"]);
      expect(errors.map((error) => error.message)).toEqual(["SSE request failed with status 401."]);
    } finally {
      resetChannelsConfig();
    }
  });
});

test.each([
  [503, "sync"],
  [410, "sync"],
  [503, "async"],
  [410, "async"],
] as const)(
  "failing recovery observer does not stop SSE retry after %i (%s)",
  async (status, mode) => {
    const observerError = new Error("observer failed");
    const diagnostic = spyOn(console, "error").mockImplementation(() => {});
    const messages: string[] = [];
    const errors: Error[] = [];
    const recoveries: string[] = [];
    let rejectObserver: ((error: Error) => void) | undefined;
    let calls = 0;
    const reader = new SseReader("http://test/events", {
      fetch: (async () => {
        if (++calls === 1) return new Response(null, { status });
        return mockFetchStream(["data: business\n\n"])();
      }) as typeof fetch,
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      backoffMaxMs: 1,
      jitter: 0,
      onRecovery(reason) {
        recoveries.push(reason);
        if (mode === "async")
          return new Promise<void>((_, reject) => {
            rejectObserver = reject;
          });
        throw observerError;
      },
      onError: (error) => errors.push(error),
      onMessage: ({ data }) => {
        messages.push(data);
        reader.close();
      },
    });
    try {
      await reader.start();
      await reader.drain();
      expect(calls).toBe(2);
      expect(recoveries).toEqual([status === 410 ? "history-expired" : "reconnecting"]);
      expect(messages).toEqual(["business"]);
      rejectObserver?.(observerError);
      await Promise.resolve();
      expect(errors).toEqual([]);
      expect(diagnostic).toHaveBeenCalledWith("SseReader onRecovery handler threw:", observerError);
    } finally {
      reader.close();
      diagnostic.mockRestore();
    }
  },
);
