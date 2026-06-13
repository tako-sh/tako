#!/usr/bin/env node
/**
 * Tako Node Worker Entrypoint — run via `npx tako-worker-node`.
 */

import { installConsoleBridge } from "../console-bridge";
import { installErrorHooks } from "../error-hooks";
import { createLogger } from "../../logger";
import { installStdioBridge } from "../stdio-bridge";
import { initBootstrapFromFd, readBootstrapData } from "../secrets-fd";
import { bootstrapWorker } from "../../workflows/bootstrap";
import { workflowsEngine } from "../../workflows/engine";

installStdioBridge("worker");
installErrorHooks("worker");
installConsoleBridge("worker");

const log = createLogger("worker");

async function main(): Promise<void> {
  initBootstrapFromFd(readBootstrapData);
  const result = await bootstrapWorker();
  if (!result.started) {
    log.error("Worker not started", { reason: result.reason ?? "unknown" });
    process.exit(result.reason === "no workflows discovered" ? 0 : 1);
  }

  let shuttingDown = false;
  const shutdown = async (reason: string): Promise<void> => {
    if (shuttingDown) return;
    shuttingDown = true;
    log.info("Shutting down", { reason });
    await workflowsEngine.drain();
    process.exit(0);
  };
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
  process.on("SIGINT", () => void shutdown("SIGINT"));

  const idleCheck = setInterval(() => {
    if (workflowsEngine.workerIdled && !shuttingDown) {
      clearInterval(idleCheck);
      void shutdown("idle");
    }
  }, 1_000);
}

void main();
