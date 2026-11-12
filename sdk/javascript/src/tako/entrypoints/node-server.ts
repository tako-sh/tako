#!/usr/bin/env node
/**
 * Tako Node.js Entrypoint — run via `npx tako-node <main>`.
 */

import { createEntrypoint } from "../create-entrypoint";
import { installConsoleBridge } from "../console-bridge";
import { installErrorHooks } from "../error-hooks";
import { installStdioBridge } from "../stdio-bridge";
import { startNodeServer } from "../node-http";
import { initBootstrapFromFd, readBootstrapData } from "../secrets-fd";
import { createGracefulShutdown } from "../shutdown";

installStdioBridge("app");
installErrorHooks("app");
installConsoleBridge("app");
initBootstrapFromFd(readBootstrapData);
const { run, host, port, setDraining, longLivedResponses } = createEntrypoint();

void run(async (handleRequest) => {
  const { actualPort, close, closeAllConnections } = await startNodeServer(
    host,
    port,
    handleRequest,
  );
  process.on(
    "SIGTERM",
    createGracefulShutdown({
      setDraining,
      responses: longLivedResponses,
      stopServer: close,
      forceClose: closeAllConnections,
    }),
  );
  return actualPort;
});
