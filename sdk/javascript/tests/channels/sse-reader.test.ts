import { describe, expect, test } from "bun:test";
import { SseReader } from "../../src/channels/sse-reader";

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
  test("dispatches messages assembled from data lines", async () => {
    const messages: string[] = [];
    const reader = new SseReader("http://x/channels/chat", {
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
    const reader = new SseReader("http://x/channels/chat", {
      fetch: mockFetchStream(["event: note\ndata: hello\ndata: world\n\n"]),
      onMessage: (message) => messages.push(`${message.type}:${message.data}`),
    });

    await reader.start();
    await reader.drain();

    expect(messages).toEqual(["note:hello\nworld"]);
  });

  test("tracks lastEventId from id lines", async () => {
    const reader = new SseReader("http://x/channels/chat", {
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
    const reader = new SseReader("http://x/channels/chat", {
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
    const reader = new SseReader("http://x/channels/chat", {
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
    const reader = new SseReader("http://x/channels/chat", {
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
    let calls = 0;
    const reader = new SseReader("http://x/channels/chat", {
      fetch: async () => {
        calls++;
        return new Response("data: ok\n\n", {
          headers: { "content-type": "text/event-stream" },
        });
      },
      onMessage: () => {},
      onError: (error) => errors.push(error.message),
      retryOnDisconnect: true,
      backoffBaseMs: 1,
      backoffMaxMs: 1,
      jitter: 0,
    });

    await reader.start();
    await reader.drain({ connections: 2 });

    expect(calls).toBe(2);
    expect(errors).toContain("SSE stream disconnected; reconnecting.");
  });
});
