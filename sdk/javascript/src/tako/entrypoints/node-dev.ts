#!/usr/bin/env node
/**
 * Tako Node.js Dev Entrypoint — HTTP + workflow worker in one process.
 */

import { installConsoleBridge } from "../console-bridge";
import { installErrorHooks } from "../error-hooks";
import { installStdioBridge } from "../stdio-bridge";
import { createEntrypoint } from "../create-entrypoint";
import { drainInProcessWorker, startInProcessWorker } from "../dev-worker";
import { startNodeServer } from "../node-http";
import { initBootstrapFromFd, readBootstrapData } from "../secrets-fd";

installStdioBridge("app");
installErrorHooks("app");
installConsoleBridge("app");
initBootstrapFromFd(readBootstrapData);
const { run, host, port, setDraining } = createEntrypoint();

void run(async (handleRequest) => {
  const { actualPort, close } = await startNodeServer(host, port, handleRequest);
  queueMicrotask(() => void startInProcessWorker());

  process.on("SIGTERM", () => {
    setDraining();
    void (async () => {
      await drainInProcessWorker();
      close();
    })();
  });

  return actualPort;
});
