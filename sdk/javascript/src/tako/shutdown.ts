/**
 * Graceful shutdown for the production HTTP entrypoints.
 *
 * tako-server sends SIGTERM once an instance has finished its ordinary
 * requests and kills it after a grace period. Server-sent event streams never
 * finish on their own, so on SIGTERM they are ended cleanly here: clients see
 * a normal end of stream and reconnect to a replacement instance instead of
 * seeing a reset when the process is killed.
 */

/**
 * Exit by this point after SIGTERM: inside the server's kill grace (15 s for
 * native processes, 10 s for containers), so the exit is ours, not a SIGKILL.
 */
export const SHUTDOWN_EXIT_DEADLINE_MS = 8_000;

/** Tracks streaming responses that only end when the instance shuts down. */
export interface LongLivedResponses {
  /** Returns `response`, or a copy whose SSE body ends when `closeAll` runs. */
  track(response: Response): Response;
  /** End every tracked stream, and any tracked later, cleanly. Returns how many were open. */
  closeAll(): number;
  /** Number of tracked streams still open. */
  readonly size: number;
}

function isEventStream(response: Response): boolean {
  const contentType = response.headers.get("content-type");
  if (!contentType) return false;
  return contentType.split(";")[0]!.trim().toLowerCase() === "text/event-stream";
}

export function createLongLivedResponses(): LongLivedResponses {
  const open = new Set<() => void>();
  let closing = false;

  function track(response: Response): Response {
    const body = response.body;
    if (!body || !isEventStream(response)) return response;

    const reader = body.getReader();
    let done = false;
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const finish = (): boolean => {
      if (done) return false;
      done = true;
      open.delete(end);
      return true;
    };
    // Close the client stream first so its end is written, then release the app's source.
    const end = () => {
      if (!finish()) return;
      controller.close();
      reader.cancel().catch(() => {});
    };
    const stream = new ReadableStream<Uint8Array>({
      start(streamController) {
        controller = streamController;
      },
      async pull(streamController) {
        let result: Awaited<ReturnType<typeof reader.read>>;
        try {
          result = await reader.read();
        } catch (error) {
          if (finish()) streamController.error(error);
          return;
        }
        if (done) return;
        if (result.done) {
          finish();
          streamController.close();
        } else {
          streamController.enqueue(result.value);
        }
      },
      cancel(reason) {
        finish();
        return reader.cancel(reason);
      },
    });
    open.add(end);
    if (closing) end();

    return new Response(stream, {
      status: response.status,
      statusText: response.statusText,
      headers: response.headers,
    });
  }

  function closeAll(): number {
    closing = true;
    const ends = [...open];
    for (const end of ends) end();
    return ends.length;
  }

  return {
    track,
    closeAll,
    get size() {
      return open.size;
    },
  };
}

export interface GracefulShutdownOptions {
  /** Mark the instance as draining in its internal status. */
  setDraining: () => void;
  /** Long-lived responses to end once SIGTERM arrives. */
  responses: LongLivedResponses;
  /** Stop accepting connections and let ordinary requests finish. */
  stopServer: () => void | Promise<void>;
  /** Close connections that are still open just before the forced exit. */
  forceClose?: () => void;
  exitDeadlineMs?: number;
  exit?: (code: number) => void;
}

/**
 * Run the shutdown sequence once. The process then exits on its own when
 * nothing else keeps it alive, so app SIGTERM handlers can still finish their
 * cleanup; a deadline timer that does not keep the process alive exits it
 * before the server would kill it.
 */
export function createGracefulShutdown(options: GracefulShutdownOptions): () => void {
  let started = false;
  return () => {
    if (started) return;
    started = true;
    options.setDraining();
    const deadline = setTimeout(() => {
      options.forceClose?.();
      if (options.exit) options.exit(0);
      else process.exit(0);
    }, options.exitDeadlineMs ?? SHUTDOWN_EXIT_DEADLINE_MS);
    deadline.unref?.();
    options.responses.closeAll();
    void Promise.resolve()
      .then(options.stopServer)
      .catch((error: unknown) => console.error("Error stopping server:", error));
  };
}
