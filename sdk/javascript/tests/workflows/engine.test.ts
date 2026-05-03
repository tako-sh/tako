import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import type { Server } from "node:net";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { WorkflowsClient } from "../../src/workflows/rpc-client";
import { WorkflowEngine } from "../../src/workflows/engine";
import { expectAsyncToThrow } from "../assertions";

function startStubServer(path: string, resp: unknown): Promise<Server> {
  return new Promise((resolve, reject) => {
    const server = createServer((socket) => {
      socket.on("data", () => {
        socket.write(`${JSON.stringify(resp)}\n`);
      });
    });
    server.once("error", reject);
    server.listen(path, () => resolve(server));
  });
}

describe("WorkflowEngine registration", () => {
  test("duplicate register throws", () => {
    const engine = new WorkflowEngine();
    engine.register("w", () => {});
    expect(() => engine.register("w", () => {})).toThrow(/already registered/);
  });

  test("registeredNames reflects registrations", () => {
    const engine = new WorkflowEngine();
    engine.register("a", () => {});
    engine.register("b", () => {});
    expect(engine.registeredNames.sort()).toEqual(["a", "b"]);
  });

  test("collectSchedules returns workflows with a schedule config", () => {
    const engine = new WorkflowEngine();
    engine.register("daily", () => {}, { schedule: "0 0 * * * *" });
    engine.register("nocron", () => {});
    expect(engine.collectSchedules()).toEqual([{ name: "daily", cron: "0 0 * * * *" }]);
  });
});

describe("WorkflowEngine enqueue (RPC)", () => {
  let dir: string;
  let sock: string;
  let server: Server | undefined;

  beforeEach(async () => {
    dir = await mkdtemp(join("/tmp", "tako-engine-"));
    sock = join(dir, "srv.sock");
  });

  afterEach(async () => {
    server?.close();
    server = undefined;
    await rm(dir, { recursive: true, force: true });
  });

  test("throws when no RPC client is configured or discoverable", async () => {
    const engine = new WorkflowEngine();
    const prevSock = process.env["TAKO_INTERNAL_SOCKET"];
    const prevApp = process.env["TAKO_APP_NAME"];
    delete process.env["TAKO_INTERNAL_SOCKET"];
    delete process.env["TAKO_APP_NAME"];
    try {
      await expectAsyncToThrow(() => engine.enqueue("w", {}), /RPC client/);
    } finally {
      if (prevSock !== undefined) process.env["TAKO_INTERNAL_SOCKET"] = prevSock;
      if (prevApp !== undefined) process.env["TAKO_APP_NAME"] = prevApp;
    }
  });

  test("delegates to the configured client", async () => {
    server = await startStubServer(sock, {
      status: "ok",
      data: { id: "srv-1", deduplicated: false },
    });
    const engine = new WorkflowEngine();
    engine.setClient(new WorkflowsClient(sock, "test-app"));
    expect(await engine.enqueue("w", { hi: 1 })).toBe("srv-1");
  });

  test("applies per-workflow retries default when caller omits it", async () => {
    let received: Record<string, unknown> | null = null;
    server = await new Promise<Server>((resolve, reject) => {
      const s = createServer((socket) => {
        socket.on("data", (chunk: Buffer) => {
          received = JSON.parse(chunk.toString().trim()) as Record<string, unknown>;
          socket.write(
            `${JSON.stringify({ status: "ok", data: { id: "x", deduplicated: false } })}\n`,
          );
        });
      });
      s.once("error", reject);
      s.listen(sock, () => resolve(s));
    });

    const engine = new WorkflowEngine();
    engine.setClient(new WorkflowsClient(sock, "test-app"));
    engine.register("w", () => {}, { retries: 6 });
    await engine.enqueue("w", {});
    const opts = (received as unknown as Record<string, Record<string, unknown>>)["opts"];
    expect(opts["max_attempts"]).toBe(7);
  });
});

describe("discover", () => {
  let dir: string;

  beforeEach(async () => {
    dir = await mkdtemp(join("/tmp", "tako-wf-"));
  });

  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  test("discovers defineWorkflow files and plain functions", async () => {
    const defineUrl = new URL("../../src/workflows/define.ts", import.meta.url).href;
    await writeFile(
      join(dir, "send-email.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("send-email", {
  retries: 4,
  schedule: "*/5 * * * *",
  handler: async (payload, step) => payload.to,
});
`,
    );
    await writeFile(join(dir, "bare.mjs"), `export default function(payload) { return "ok"; }`);
    await writeFile(join(dir, "_ignored.mjs"), `export default () => {};`);

    const engine = new WorkflowEngine();
    const count = await engine.discover(dir);
    expect(count).toBe(2);
    expect(engine.registeredNames.sort()).toEqual(["bare", "send-email"]);
  });

  test("can discover only workflows assigned to a worker group", async () => {
    const defineUrl = new URL("../../src/workflows/define.ts", import.meta.url).href;
    await writeFile(
      join(dir, "default-job.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("default-job", { handler: async () => {} });
`,
    );
    await writeFile(
      join(dir, "media-job.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("media-job", { worker: "media", handler: async () => {} });
`,
    );

    const engine = new WorkflowEngine();
    const count = await engine.discover(dir, { worker: "media" });
    expect(count).toBe(1);
    expect(engine.registeredNames).toEqual(["media-job"]);
  });

  test("default worker group discovers workflows without an explicit worker", async () => {
    const defineUrl = new URL("../../src/workflows/define.ts", import.meta.url).href;
    await writeFile(
      join(dir, "default-job.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("default-job", { handler: async () => {} });
`,
    );
    await writeFile(
      join(dir, "media-job.mjs"),
      `
import { defineWorkflow } from "${defineUrl}";
export default defineWorkflow("media-job", { worker: "media", handler: async () => {} });
`,
    );

    const engine = new WorkflowEngine();
    const count = await engine.discover(dir, { worker: "default" });
    expect(count).toBe(1);
    expect(engine.registeredNames).toEqual(["default-job"]);
  });

  test("missing directory returns 0 and does not throw", async () => {
    const engine = new WorkflowEngine();
    const count = await engine.discover(join("/tmp", "tako-nonexistent-" + Date.now()));
    expect(count).toBe(0);
  });

  test("rejects files without a default function export", async () => {
    await writeFile(join(dir, "bad.mjs"), `export const foo = 1;`);
    const engine = new WorkflowEngine();
    await expectAsyncToThrow(() => engine.discover(dir), /defineWorkflow.*or a plain function/);
  });
});
