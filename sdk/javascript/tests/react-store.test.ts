import { afterAll, afterEach, expect, test } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { createElement, StrictMode, useLayoutEffect } from "react";
import { renderToString } from "react-dom/server";
import { useStore } from "../src/react/store";

const registered = typeof document === "undefined";
if (registered) GlobalRegistrator.register();

// Later test files in the same run expect Bun's own globals, such as fetch.
afterAll(async () => {
  if (registered) await GlobalRegistrator.unregister();
});

interface PendingRead {
  signal: AbortSignal | null | undefined;
  resolve(data: unknown, offline?: { maxAgeMs: number; version: string }): void;
}
class Backend {
  readonly reads: PendingRead[] = [];
  readonly streams: Array<{ send(type: string): void; close(): void }> = [];
  readonly fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input));
    if (url.pathname.startsWith("/_tako/channels/")) {
      let closed = false;
      let controller!: ReadableStreamDefaultController<Uint8Array>;
      const stream = new ReadableStream<Uint8Array>({
        start(value) {
          controller = value;
        },
        cancel() {
          closed = true;
        },
      });
      const close = () => {
        if (!closed) {
          closed = true;
          controller.close();
        }
      };
      init?.signal?.addEventListener("abort", close, { once: true });
      this.streams.push({
        close,
        send: (type) => {
          if (!closed)
            controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify({ type })}\n\n`));
        },
      });
      return new Response(stream, { headers: { "Content-Type": "text/event-stream" } });
    }
    return new Promise<Response>((resolve) => {
      this.reads.push({
        signal: init?.signal,
        resolve: (data, offline) => resolve(Response.json({ data, ...(offline && { offline }) })),
      });
    });
  }) as typeof fetch;
}

afterEach(cleanup);

test("useStore clears old scope synchronously during render and ignores late old responses", async () => {
  const backend = new Backend();
  const observed: Array<{ room: string; data: string | undefined }> = [];
  const { result, rerender } = renderHook(
    ({ room }) => {
      const value = useStore<string>("messages", {
        baseUrl: "https://app.test",
        params: { room },
        fetch: backend.fetch,
      });
      useLayoutEffect(() => {
        observed.push({ room, data: value.data });
      }, [room, value.data]);
      return value;
    },
    { initialProps: { room: "first" } },
  );
  await act(async () => {
    backend.reads[0]!.resolve("first room data");
  });
  expect(result.current.data).toBe("first room data");
  rerender({ room: "second" });
  expect(result.current.data).toBeUndefined();
  expect(
    observed
      .filter((value) => value.room === "second")
      .every((value) => value.data !== "first room data"),
  ).toBe(true);
  expect(backend.reads[0]!.signal?.aborted).toBe(false);
  await act(async () => {
    backend.reads[1]!.resolve("second room data");
  });
  expect(result.current.data).toBe("second room data");
});

test("credential and origin changes cannot expose previous identity data", async () => {
  const backend = new Backend();
  const { result, rerender } = renderHook(
    ({ authorization, baseUrl, cacheScope }) =>
      useStore<string>("messages", {
        baseUrl,
        authorization,
        cacheScope,
        fetch: backend.fetch,
      }),
    { initialProps: { authorization: "alice", baseUrl: "https://one.test", cacheScope: "alice" } },
  );
  await act(async () => {
    backend.reads[0]!.resolve("alice data");
  });
  expect(result.current.data).toBe("alice data");
  rerender({ authorization: "bob", baseUrl: "https://one.test", cacheScope: "bob" });
  expect(result.current.data).toBeUndefined();
  await act(async () => {
    backend.reads[1]!.resolve("bob data");
  });
  expect(result.current.data).toBe("bob data");
  rerender({ authorization: "bob", baseUrl: "https://two.test", cacheScope: "bob" });
  expect(result.current.data).toBeUndefined();
  await act(async () => {
    backend.reads[2]!.resolve("other backend data");
  });
  expect(result.current.data).toBe("other backend data");
});

test("old in-flight responses and old refresh callbacks cannot update a new identity", async () => {
  const backend = new Backend();
  const { result, rerender } = renderHook(
    ({ room }) =>
      useStore<string>("messages", {
        baseUrl: "https://app.test",
        params: { room },
        fetch: backend.fetch,
      }),
    { initialProps: { room: "first" } },
  );
  const oldRefresh = result.current.refresh;
  rerender({ room: "second" });
  expect(backend.reads[0]!.signal?.aborted).toBe(true);
  await act(async () => {
    backend.reads[0]!.resolve("old late data");
  });
  expect(result.current.data).toBeUndefined();
  await act(async () => {
    backend.reads[1]!.resolve("new data");
  });
  expect(result.current.data).toBe("new data");
  const oldRead = oldRefresh();
  expect(backend.reads).toHaveLength(3);
  await act(async () => {
    backend.reads[2]!.resolve("old identity refreshed");
    await oldRead;
  });
  expect(result.current.data).toBe("new data");
});

test("semantically unchanged options do not reconnect, and disabling clears data", async () => {
  const backend = new Backend();
  const { result, rerender } = renderHook(
    ({ enabled, flip }) =>
      useStore<number>("messages", {
        baseUrl: "https://app.test",
        params: flip ? { b: 2, a: 1 } : { a: 1, b: 2 },
        enabled,
        fetch: backend.fetch,
      }),
    { initialProps: { enabled: true, flip: false } },
  );
  await act(async () => {
    backend.reads[0]!.resolve(42);
  });
  rerender({ enabled: true, flip: true });
  expect(result.current.data).toBe(42);
  expect(backend.reads).toHaveLength(1);
  rerender({ enabled: false, flip: true });
  expect(result.current.data).toBeUndefined();
  expect(backend.reads).toHaveLength(1);
});

test("Strict Mode subscribes safely and unmount aborts the surviving request", async () => {
  const backend = new Backend();
  const { result, unmount } = renderHook(
    () =>
      useStore<string>("messages", {
        baseUrl: "https://app.test",
        fetch: backend.fetch,
      }),
    { wrapper: ({ children }) => createElement(StrictMode, null, children) },
  );
  await waitFor(() => expect(backend.reads.length).toBeGreaterThanOrEqual(1));
  for (const discarded of backend.reads.slice(0, -1)) expect(discarded.signal?.aborted).toBe(true);
  const live = backend.reads.at(-1)!;
  await act(async () => {
    live.resolve("live");
  });
  expect(result.current.data).toBe("live");
  const pending = result.current.refresh();
  const request = backend.reads.at(-1)!;
  unmount();
  expect(request.signal?.aborted).toBe(true);
  request.resolve("after unmount");
  await pending;
});

test("server rendering starts no requests and returns a deterministic loading snapshot", () => {
  const backend = new Backend();
  function Page() {
    const value = useStore<string>("messages", { fetch: backend.fetch });
    return createElement("span", null, value.status);
  }
  expect(renderToString(createElement(Page))).toBe("<span>loading</span>");
  expect(backend.reads).toHaveLength(0);
});

test("useStore retains valid cached data quietly and uses the latest recovery callback", async () => {
  const backend = new Backend();
  const callbacks: string[] = [];
  const { result, rerender, unmount } = renderHook(
    ({ room, label }) =>
      useStore<string>("messages", {
        baseUrl: "https://app.test",
        params: { room },
        fetch: backend.fetch,
        onRecovery: (reason) => callbacks.push(`${label}:${reason}`),
      }),
    { initialProps: { room: "first", label: "original" } },
  );
  await act(async () => {
    backend.reads[0]!.resolve("cached", { maxAgeMs: 10_000, version: "1" });
    backend.streams[0]!.send("tako.store.ready");
  });
  await waitFor(() => expect(backend.reads).toHaveLength(2));
  await act(async () => {
    backend.reads[1]!.resolve("cached", { maxAgeMs: 10_000, version: "1" });
  });
  expect(result.current.isStale).toBe(false);
  rerender({ room: "first", label: "latest" });
  expect(backend.streams).toHaveLength(1);
  await act(async () => {
    backend.streams[0]!.close();
  });
  expect(result.current).toMatchObject({
    data: "cached",
    status: "offline",
    isStale: true,
    error: null,
  });
  expect(callbacks).toEqual(["latest:reconnecting"]);
  rerender({ room: "second", label: "new identity" });
  expect(result.current.data).toBeUndefined();
  expect(backend.streams).toHaveLength(2);
  await act(async () => {
    backend.streams[0]!.send("tako.store.ready");
    backend.reads[2]!.resolve("new data", { maxAgeMs: 10_000, version: "1" });
  });
  expect(result.current.data).toBe("new data");
  expect(callbacks).toEqual(["latest:reconnecting"]);
  unmount();
});

test("useStore exposes an unavailable snapshot after its offline lease expires", async () => {
  const backend = new Backend();
  const { result, unmount } = renderHook(() =>
    useStore<string>("messages", {
      baseUrl: "https://app.test",
      fetch: backend.fetch,
    }),
  );
  await act(async () => {
    backend.reads[0]!.resolve("cached", { maxAgeMs: 100, version: "1" });
    backend.streams[0]!.close();
  });
  expect(result.current.data).toBe("cached");
  expect(result.current.error).toBeNull();
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 120));
  });
  expect(result.current.data).toBeUndefined();
  expect(result.current.error).not.toBeNull();
  unmount();
});
