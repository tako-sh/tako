#!/usr/bin/env node
/**
 * Tako Node.js Entrypoint — run via `npx tako-node <main>`.
 */

import { createEntrypoint } from "../create-entrypoint";
import { installConsoleBridge } from "../console-bridge";
import { installErrorHooks } from "../error-hooks";
import { installStdioBridge } from "../stdio-bridge";
import { startNodeServer } from "../node-http";
import { initBootstrapFromFd, readViaInheritedFd } from "../secrets-fd";

installStdioBridge("app");
installErrorHooks("app");
installConsoleBridge("app");
initBootstrapFromFd(readViaInheritedFd);
const { run, host, port, setDraining } = createEntrypoint();

void run(async (handleRequest) => {
  const { actualPort, close } = await startNodeServer(host, port, handleRequest);
  process.on("SIGTERM", () => {
    setDraining();
    close();
  });
  return actualPort;
});
