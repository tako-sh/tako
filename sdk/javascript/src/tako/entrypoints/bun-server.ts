#!/usr/bin/env bun
/**
 * Tako Bun Entrypoint — run via `bunx tako-bun <main>`
 *
 * HTTP-serving mode only. The task/workflow engine runs in a separate
 * worker process (`bunx tako-worker`) spawned by tako-server.
 */

import { createEntrypoint } from "../create-entrypoint";
import { installConsoleBridge } from "../console-bridge";
import { installErrorHooks } from "../error-hooks";
import { installStdioBridge } from "../stdio-bridge";
import { initBootstrapFromFd, readBootstrapData } from "../secrets-fd";
import { createGracefulShutdown } from "../shutdown";

installStdioBridge("app");
installErrorHooks("app");
installConsoleBridge("app");
initBootstrapFromFd(readBootstrapData);

const { run, host, port, setDraining, longLivedResponses } = createEntrypoint();

if (import.meta.main) {
  let server: ReturnType<typeof Bun.serve> | undefined;

  void run((handleRequest) => {
    server = Bun.serve({ hostname: host, port, fetch: handleRequest });
    return server.port;
  });

  process.on(
    "SIGTERM",
    createGracefulShutdown({
      setDraining,
      responses: longLivedResponses,
      stopServer: () => server?.stop(),
      forceClose: () => void server?.stop(true),
    }),
  );
}
