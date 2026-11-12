import { expect, test } from "bun:test";
import { terminalClose } from "../../src/react/reconnect";
import { startNodeServer } from "../../src/tako/node-http";
import { createGracefulShutdown } from "../../src/tako/shutdown";

const encoder = new TextEncoder();

/**
 * An SSE response that sends one event at once and the rest only when
 * `finish` runs, like a token stream still generating an answer. Without
 * `finish` it never ends, like a live event feed.
 */
function eventStream(): { response: Response; finish: () => void } {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const body = new ReadableStream<Uint8Array>({
    start(streamController) {
      controller = streamController;
      controller.enqueue(encoder.encode("data: first\n\n"));
    },
  });
  return {
    response: new Response(body, {
      headers: { "content-type": "text/event-stream; charset=utf-8" },
    }),
    finish: () => {
      controller.enqueue(encoder.encode("data: last\n\n"));
      controller.close();
    },
  };
}

async function readFirstChunk(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<string> {
  const { value } = await reader.read();
  return new TextDecoder().decode(value);
}

/** Read the rest of the stream: resolves with the text on a clean end, rejects on an abrupt one. */
async function readToEnd(reader: ReadableStreamDefaultReader<Uint8Array>): Promise<string> {
  const decoder = new TextDecoder();
  let text = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return text;
    text += decoder.decode(value, { stream: true });
  }
}

function within<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  return Promise.race([
    promise,
    new Promise<T>((_, reject) => setTimeout(() => reject(new Error(`timed out: ${what}`)), ms)),
  ]);
}

/** Settles `promise` as "done", "error" or "pending" after `ms`. */
async function settledWithin(promise: Promise<unknown>, ms: number): Promise<string> {
  return Promise.race([
    promise.then(
      () => "done",
      () => "error",
    ),
    Bun.sleep(ms).then(() => "pending"),
  ]);
}

interface Harness {
  url: string;
  stopServer: () => void | Promise<void>;
  forceClose: () => void;
  cleanup: () => void | Promise<void>;
}

const servers: Record<
  string,
  (fetch: (request: Request) => Promise<Response>) => Promise<Harness>
> = {
  bun: async (fetch) => {
    const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch });
    return {
      url: `http://127.0.0.1:${server.port}`,
      stopServer: () => server.stop(),
      forceClose: () => void server.stop(true),
      cleanup: () => server.stop(true),
    };
  },
  node: async (fetch) => {
    const { actualPort, close, closeAllConnections } = await startNodeServer("127.0.0.1", 0, fetch);
    return {
      url: `http://127.0.0.1:${actualPort}`,
      stopServer: close,
      forceClose: closeAllConnections,
      cleanup: closeAllConnections,
    };
  },
};

for (const [runtime, start] of Object.entries(servers)) {
  test(`${runtime}: SIGTERM lets an event stream and a slow request finish before the deadline`, async () => {
    const streams: Array<() => void> = [];
    let releaseSlow!: () => void;
    const slowGate = new Promise<void>((resolve) => (releaseSlow = resolve));
    const harness = await start(async (request) => {
      if (new URL(request.url).pathname === "/slow") {
        await slowGate;
        return new Response("slow done");
      }
      const { response, finish } = eventStream();
      streams.push(finish);
      return response;
    });
    const exits: number[] = [];
    let forced = 0;
    let draining = 0;
    const shutdown = createGracefulShutdown({
      setDraining: () => draining++,
      stopServer: harness.stopServer,
      forceClose: () => {
        forced++;
        harness.forceClose();
      },
      exitDeadlineMs: 5_000,
      exit: (code) => exits.push(code),
    });

    try {
      const events = await fetch(`${harness.url}/events`);
      const reader = events.body!.getReader();
      expect(await readFirstChunk(reader)).toBe("data: first\n\n");
      const slow = fetch(`${harness.url}/slow`);
      await Bun.sleep(20);

      shutdown();
      shutdown();
      expect(draining).toBe(1);

      // The stream is not ended for the app: it stays open until the app finishes it.
      const rest = readToEnd(reader);
      expect(await settledWithin(rest, 200)).toBe("pending");

      streams[0]!();
      expect(await within(rest, 2_000, "stream end")).toBe("data: last\n\n");
      releaseSlow();
      expect(await (await within(slow, 2_000, "slow request")).text()).toBe("slow done");
      expect(forced).toBe(0);
      expect(exits).toEqual([]);
    } finally {
      releaseSlow();
      await harness.cleanup();
    }
  });

  test(`${runtime}: SIGTERM force-closes a stream still open at the deadline, never cleanly`, async () => {
    const harness = await start(async () => eventStream().response);
    const exits: number[] = [];
    const shutdown = createGracefulShutdown({
      setDraining: () => {},
      stopServer: harness.stopServer,
      forceClose: harness.forceClose,
      exitDeadlineMs: 300,
      exit: (code) => exits.push(code),
    });

    try {
      const events = await fetch(`${harness.url}/events`);
      const reader = events.body!.getReader();
      expect(await readFirstChunk(reader)).toBe("data: first\n\n");

      shutdown();
      const rest = readToEnd(reader);
      expect(await settledWithin(rest, 150)).toBe("pending");
      expect(await settledWithin(rest, 2_000)).toBe("error");
      expect(exits).toEqual([0]);
    } finally {
      await harness.cleanup();
    }
  });
}

test("shutdown force-closes and exits at the deadline when the server does not stop", async () => {
  const exits: number[] = [];
  let forced = 0;
  const shutdown = createGracefulShutdown({
    setDraining: () => {},
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
