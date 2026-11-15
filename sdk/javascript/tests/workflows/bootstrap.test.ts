import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import type { Server, Socket } from "node:net";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { closeInternalSocketPoolsForTests } from "../../src/tako/socket";
import { bootstrapWorker } from "../../src/workflows/bootstrap";
import { workflowsEngine } from "../../src/workflows/engine";

function startRpcCaptureServer(path: string, commands: unknown[]): Promise<Server> {
  return new Promise((resolve, reject) => {
    const server = createServer((socket: Socket) => {
      let buffer = "";
      socket.on("data", (chunk: Buffer) => {
        buffer += chunk.toString("utf8");
        let newline: number;
        while ((newline = buffer.indexOf("\n")) !== -1) {
          const line = buffer.slice(0, newline);
          buffer = buffer.slice(newline + 1);
          commands.push(JSON.parse(line));
          socket.write(`${JSON.stringify({ status: "ok", data: {} })}\n`);
        }
      });
    });
    server.once("error", reject);
    server.listen(path, () => resolve(server));
  });
}

describe("bootstrapWorker", () => {
  let dir: string;
  let sock: string;
  let server: Server | undefined;
  let previousSocket: string | undefined;
  let previousApp: string | undefined;
  let previousGroup: string | undefined;
  let previousStartWorker: typeof workflowsEngine.startWorker;

  beforeEach(async () => {
    closeInternalSocketPoolsForTests();
    workflowsEngine._reset();
    dir = await mkdtemp(join("/tmp", "tako-bootstrap-worker-"));
    sock = join(dir, "internal.sock");
    previousSocket = process.env["TAKO_INTERNAL_SOCKET"];
    previousApp = process.env["TAKO_APP_NAME"];
    previousGroup = process.env["TAKO_WORKFLOW_WORKER"];
    delete process.env["TAKO_WORKFLOW_WORKER"];
    process.env["TAKO_INTERNAL_SOCKET"] = sock;
    process.env["TAKO_APP_NAME"] = "test-app";
    previousStartWorker = workflowsEngine.startWorker.bind(workflowsEngine);
    workflowsEngine.startWorker = (() => {}) as typeof workflowsEngine.startWorker;
  });

  afterEach(async () => {
    workflowsEngine.startWorker = previousStartWorker;
    workflowsEngine._reset();
    closeInternalSocketPoolsForTests();
    server?.close();
    server = undefined;
    if (previousSocket === undefined) delete process.env["TAKO_INTERNAL_SOCKET"];
    else process.env["TAKO_INTERNAL_SOCKET"] = previousSocket;
    if (previousApp === undefined) delete process.env["TAKO_APP_NAME"];
    else process.env["TAKO_APP_NAME"] = previousApp;
    if (previousGroup === undefined) delete process.env["TAKO_WORKFLOW_WORKER"];
    else process.env["TAKO_WORKFLOW_WORKER"] = previousGroup;
    await rm(dir, { recursive: true, force: true });
  });

  test("registers an empty schedule set when discovered workflows have no cron", async () => {
    const workflowsDir = join(dir, "workflows");
    await mkdir(workflowsDir);
    const defineUrl = new URL("../../src/workflows/define.ts", import.meta.url).href;
    await writeFile(
      join(workflowsDir, "manual.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("manual", { handler: async () => {} });
`,
    );

    const commands: unknown[] = [];
    server = await startRpcCaptureServer(sock, commands);

    const result = await bootstrapWorker({ appDir: dir, appRoot: "." });

    expect(result).toEqual({ started: true, workflowCount: 1 });
    expect(commands).toEqual([
      {
        command: "register_schedules",
        app: "test-app",
        schedules: [],
      },
    ]);
  });

  test("schedules carry the workflow's retry budget", async () => {
    const workflowsDir = join(dir, "workflows");
    await mkdir(workflowsDir);
    const defineUrl = new URL("../../src/workflows/define.ts", import.meta.url).href;
    await writeFile(
      join(workflowsDir, "nightly.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("nightly", { schedule: "0 0 * * *", retries: 4, handler: async () => {} });
`,
    );
    await writeFile(
      join(workflowsDir, "hourly.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("hourly", { schedule: "0 * * * *", handler: async () => {} });
`,
    );

    const commands: Array<{ schedules?: unknown[] }> = [];
    server = await startRpcCaptureServer(sock, commands);

    await bootstrapWorker({ appDir: dir, appRoot: "." });

    expect(commands[0]?.schedules).toHaveLength(2);
    expect(commands[0]?.schedules).toContainEqual({
      name: "nightly",
      cron: "0 0 * * *",
      max_attempts: 5,
    });
    // Without `retries`, the server default budget applies.
    expect(commands[0]?.schedules).toContainEqual({ name: "hourly", cron: "0 * * * *" });
  });

  test("every group registers all schedules and only claims its own workflows", async () => {
    const workflowsDir = join(dir, "workflows");
    await mkdir(workflowsDir);
    const defineUrl = new URL("../../src/workflows/define.ts", import.meta.url).href;
    await writeFile(
      join(workflowsDir, "email.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("email", { worker: "email", schedule: "0 9 * * 1-5", handler: async () => {} });
`,
    );
    const commands: unknown[] = [];
    server = await startRpcCaptureServer(sock, commands);

    for (const group of ["default", "email"]) {
      process.env["TAKO_WORKFLOW_WORKER"] = group;
      workflowsEngine._reset();
      const result = await bootstrapWorker({ appDir: dir, appRoot: "." });
      expect(result.started).toBe(group === "email");
      expect(workflowsEngine.registeredNames).toEqual(group === "email" ? ["email"] : []);
    }
    const registration = {
      command: "register_schedules",
      app: "test-app",
      schedules: [{ name: "email", cron: "0 9 * * 1-5" }],
    };
    expect(commands).toEqual([registration, registration]);
  });
});
