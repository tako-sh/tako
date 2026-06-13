#!/usr/bin/env bun
/**
 * Tako Bun Dev Entrypoint — runs HTTP + workflow worker in one process.
 */

import { installConsoleBridge } from "../console-bridge";
import { installErrorHooks } from "../error-hooks";
import { installStdioBridge } from "../stdio-bridge";
import { createEntrypoint } from "../create-entrypoint";
import { drainInProcessWorker, startInProcessWorker } from "../dev-worker";
import { initBootstrapFromFd, readBootstrapData } from "../secrets-fd";

installStdioBridge("app");
installErrorHooks("app");
installConsoleBridge("app");
initBootstrapFromFd(readBootstrapData);
const { run, host, port, setDraining } = createEntrypoint();

if (import.meta.main) {
  let server: ReturnType<typeof Bun.serve> | undefined;

  void run(async (handleRequest) => {
    server = Bun.serve({ hostname: host, port, fetch: handleRequest });
    queueMicrotask(() => void startInProcessWorker());
    return server.port;
  });

  process.on("SIGTERM", () => {
    setDraining();
    void (async () => {
      await drainInProcessWorker();
      void server?.stop(true);
    })();
  });
}
