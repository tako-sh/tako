/**
 * Graceful shutdown for the production HTTP entrypoints.
 *
 * tako-server sends SIGTERM once an instance has finished its ordinary
 * requests and event streams (or its drain timed out), and kills it after a
 * grace period. On SIGTERM the server stops accepting connections and lets
 * in-flight requests and streams finish on their own. Whatever is still open
 * at the deadline is closed abruptly, never ended cleanly: a client must not
 * mistake a cut-off response for a complete one, and treats the error as a
 * reason to retry or reconnect.
 */

/**
 * Exit by this point after SIGTERM: inside the server's kill grace (15 s for
 * native processes, 10 s for containers), so the exit is ours, not a SIGKILL.
 */
export const SHUTDOWN_EXIT_DEADLINE_MS = 8_000;

export interface GracefulShutdownOptions {
  /** Mark the instance as draining in its internal status. */
  setDraining: () => void;
  /** Stop accepting connections and let in-flight requests and streams finish. */
  stopServer: () => void | Promise<void>;
  /** Abruptly close connections that are still open just before the forced exit. */
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
    void Promise.resolve()
      .then(options.stopServer)
      .catch((error: unknown) => console.error("Error stopping server:", error));
  };
}
