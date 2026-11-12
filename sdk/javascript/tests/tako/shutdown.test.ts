import { expect, test } from "bun:test";
import { terminalClose } from "../../src/react/reconnect";
import { startNodeServer } from "../../src/tako/node-http";
import { createGracefulShutdown, createLongLivedResponses } from "../../src/tako/shutdown";

const encoder = new TextEncoder();

/** An SSE response that only ends when cancelled, like a live event feed. */
function openEventStream(onCancel: () => void): Response {
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode("data: hello\n\n"));
    },
    cancel() {
      onCancel();
    },
  });
  return new Response(body, { headers: { "content-type": "text/event-stream; charset=utf-8" } });
}

async function readFirstChunk(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<string> {
  const { value } = await reader.read();
  return new TextDecoder().decode(value);
}

async function readToEnd(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<void> {
  for (;;) {
    const { done } = await reader.read();
    if (done) return;
  }
}

function within<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  return Promise.race([
    promise,
    new Promise<T>((_, reject) => setTimeout(() => reject(new Error(`timed out: ${what}`)), ms)),
  ]);
}

test("only event-stream responses are tracked", () => {
  const responses = createLongLivedResponses();
  const plain = new Response("ok");
  expect(responses.track(plain)).toBe(plain);
  const empty = new Response(null, { headers: { "content-type": "text/event-stream" } });
  expect(responses.track(empty)).toBe(empty);
  expect(responses.size).toBe(0);

  const sse = responses.track(openEventStream(() => {}));
  expect(sse.headers.get("content-type")).toBe("text/event-stream; charset=utf-8");
  expect(responses.size).toBe(1);
});

test("a stream that ends on its own stops being tracked", async () => {
  const responses = createLongLivedResponses();
  const tracked = responses.track(
    new Response("data: done\n\n", { headers: { "content-type": "text/event-stream" } }),
  );
  expect(await tracked.text()).toBe("data: done\n\n");
  expect(responses.size).toBe(0);
  expect(responses.closeAll()).toBe(0);
});

test("closeAll ends open streams cleanly and releases the app source", async () => {
  const responses = createLongLivedResponses();
  let cancelled = 0;
  const reader = responses.track(openEventStream(() => cancelled++)).body!.getReader();
  expect(await readFirstChunk(reader)).toBe("data: hello\n\n");

  expect(responses.closeAll()).toBe(1);
  await within(readToEnd(reader), 1_000, "stream end");
  expect(cancelled).toBe(1);
  expect(responses.size).toBe(0);

  // A stream that starts after shutdown began ends immediately.
  const late = responses.track(openEventStream(() => cancelled++));
  expect(await late.text()).toBe("");
  expect(cancelled).toBe(2);
});

test("bun server SIGTERM path ends SSE cleanly and lets short requests finish", async () => {
  const responses = createLongLivedResponses();
  let releaseSlow!: () => void;
  const slowGate = new Promise<void>((resolve) => (releaseSlow = resolve));
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch: async (request) => {
      if (new URL(request.url).pathname === "/slow") {
        await slowGate;
        return new Response("slow done");
      }
      return responses.track(openEventStream(() => {}));
    },
  });
  const exits: number[] = [];
  let drained = false;
  const shutdown = createGracefulShutdown({
    setDraining: () => (drained = true),
    responses,
    stopServer: () => server.stop(),
    forceClose: () => void server.stop(true),
    exit: (code) => exits.push(code),
  });

  try {
    const events = await fetch(`http://127.0.0.1:${server.port}/events`);
    const reader = events.body!.getReader();
    expect(await readFirstChunk(reader)).toBe("data: hello\n\n");
    const slow = fetch(`http://127.0.0.1:${server.port}/slow`);
    await Bun.sleep(20);

    shutdown();
    shutdown();
    expect(drained).toBe(true);
    await within(readToEnd(reader), 2_000, "SSE end");

    releaseSlow();
    expect(await (await within(slow, 2_000, "slow request")).text()).toBe("slow done");
    expect(exits).toEqual([]);
  } finally {
    releaseSlow();
    await server.stop(true);
  }
});

test("node server SIGTERM path ends SSE cleanly and the server closes promptly", async () => {
  const responses = createLongLivedResponses();
  const { actualPort, close, closeAllConnections } = await startNodeServer(
    "127.0.0.1",
    0,
    async () => responses.track(openEventStream(() => {})),
  );
  let stopped = false;
  const shutdown = createGracefulShutdown({
    setDraining: () => {},
    responses,
    stopServer: () => close().then(() => void (stopped = true)),
    forceClose: closeAllConnections,
    exit: () => {},
  });

  try {
    const events = await fetch(`http://127.0.0.1:${actualPort}/events`);
    const reader = events.body!.getReader();
    expect(await readFirstChunk(reader)).toBe("data: hello\n\n");

    shutdown();
    await within(readToEnd(reader), 2_000, "SSE end");
    const started = Date.now();
    while (!stopped && Date.now() - started < 2_000) await Bun.sleep(10);
    expect(stopped).toBe(true);
  } finally {
    closeAllConnections();
  }
});

test("shutdown force-closes and exits at the deadline when connections stay open", async () => {
  const exits: number[] = [];
  let forced = 0;
  const shutdown = createGracefulShutdown({
    setDraining: () => {},
    responses: createLongLivedResponses(),
    stopServer: () => new Promise<void>(() => {}),
    forceClose: () => forced++,
    exitDeadlineMs: 20,
    exit: (code) => exits.push(code),
  });

  shutdown();
  await Bun.sleep(60);
  expect(forced).toBe(1);
  expect(exits).toEqual([0]);
});

test("service restart close code 1012 is retryable for SDK reconnecting clients", () => {
  expect(terminalClose(1012)).toBe(false);
});
