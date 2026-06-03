import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import type { Server, Socket } from "node:net";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { closeInternalSocketPoolsForTests } from "../../src/tako/socket";
import { WorkflowsClient } from "../../src/workflows/rpc-client";
import type { Run } from "../../src/workflows/types";
import type { RegisteredWorkflow, WorkflowHandler } from "../../src/workflows/worker";
import { Worker } from "../../src/workflows/worker";

class MockServer {
  server!: Server;
  path = "";
  private tasks: Run[] = [];
  private idCounter = 0;

  async start(dir: string): Promise<void> {
    this.path = join(dir, "srv.sock");
    this.server = createServer((socket: Socket) => this.handleConnection(socket));
    await new Promise<void>((r) => this.server.listen(this.path, r));
  }

  async close(): Promise<void> {
    await new Promise<void>((r) => this.server.close(() => r()));
  }

  seed(task: Partial<Run> & { name: string }): string {
    const id = `t${++this.idCounter}`;
    this.tasks.push({
      id,
      name: task.name,
      payload: task.payload ?? {},
      status: "pending",
      attempts: 0,
      retries: task.retries ?? 2,
      runAt: task.runAt ?? Date.now(),
      leaseUntil: null,
      workerId: null,
      lastError: null,
      stepState: task.stepState ?? {},
      createdAt: Date.now(),
      uniqueKey: null,
    });
    return id;
  }

  find(id: string): Run | undefined {
    return this.tasks.find((t) => t.id === id);
  }

  private handleConnection(socket: Socket): void {
    let buf = "";
    socket.on("data", (chunk: Buffer) => {
      buf += chunk.toString("utf8");
      let nl: number;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        try {
          const cmd = JSON.parse(line) as Record<string, unknown>;
          const resp = this.dispatch(cmd);
          socket.write(`${JSON.stringify(resp)}\n`);
        } catch (err) {
          socket.write(`${JSON.stringify({ status: "error", message: String(err) })}\n`);
        }
      }
    });
  }

  private dispatch(cmd: Record<string, unknown>): unknown {
    switch (cmd["command"]) {
      case "claim_run": {
        const names = cmd["names"] as string[];
        const task = this.tasks.find(
          (t) => t.status === "pending" && names.includes(t.name) && t.runAt <= Date.now(),
        );
        if (!task) return { status: "ok", data: null };
        task.status = "running";
        task.attempts += 1;
        task.workerId = cmd["worker_id"] as string;
        return {
          status: "ok",
          data: {
            id: task.id,
            name: task.name,
            payload: task.payload,
            status: task.status,
            attempts: task.attempts,
            max_attempts: task.retries + 1,
            run_at_ms: task.runAt,
            step_state: task.stepState,
          },
        };
      }
      case "heartbeat_run":
        return { status: "ok", data: {} };
      case "save_step": {
        const task = this.find(cmd["id"] as string);
        if (task) {
          // Steps table model: append (step_name, result) per call.
          const stepName = cmd["step_name"] as string;
          task.stepState = { ...task.stepState, [stepName]: cmd["result"] };
        }
        return { status: "ok", data: {} };
      }
      case "complete_run": {
        const task = this.find(cmd["id"] as string);
        if (task) {
          task.status = "succeeded";
          task.workerId = null;
        }
        return { status: "ok", data: {} };
      }
      case "cancel_run": {
        const task = this.find(cmd["id"] as string);
        if (task) {
          task.status = "cancelled";
          task.lastError = (cmd["reason"] as string) ?? null;
          task.workerId = null;
        }
        return { status: "ok", data: {} };
      }
      case "defer_run": {
        const task = this.find(cmd["id"] as string);
        if (task) {
          task.status = "pending";
          task.runAt = (cmd["wake_at_ms"] as number) ?? Number.MAX_SAFE_INTEGER;
          task.workerId = null;
        }
        return { status: "ok", data: {} };
      }
      case "wait_for_event": {
        const task = this.find(cmd["id"] as string);
        if (task) {
          task.status = "pending";
          task.runAt = (cmd["timeout_at_ms"] as number) ?? Number.MAX_SAFE_INTEGER;
          task.workerId = null;
        }
        return { status: "ok", data: {} };
      }
      case "fail_run": {
        const task = this.find(cmd["id"] as string);
        if (task) {
          if (cmd["finalize"]) {
            task.status = "dead";
          } else {
            task.status = "pending";
            task.runAt = (cmd["next_run_at_ms"] as number) ?? Date.now();
          }
          task.lastError = cmd["error"] as string;
          task.workerId = null;
        }
        return { status: "ok", data: {} };
      }
      default:
        return { status: "error", message: `unknown: ${String(cmd["command"])}` };
    }
  }
}

function registry(handlers: Record<string, WorkflowHandler>): Map<string, RegisteredWorkflow> {
  return new Map(Object.entries(handlers).map(([name, handler]) => [name, { handler }]));
}

describe("Worker", () => {
  let dir: string;
  let mock: MockServer;
  let client: WorkflowsClient;

  beforeEach(async () => {
    closeInternalSocketPoolsForTests();
    dir = await mkdtemp(join("/tmp", "tako-worker-"));
    mock = new MockServer();
    await mock.start(dir);
    client = new WorkflowsClient(mock.path);
  });

  afterEach(async () => {
    closeInternalSocketPoolsForTests();
    await mock.close();
    await rm(dir, { recursive: true, force: true });
  });

  test("processes one task and marks it succeeded", async () => {
    const seen: unknown[] = [];
    const worker = new Worker({
      client,
      workerId: "w1",
      registry: registry({
        echo: (p) => {
          seen.push(p);
          return "ok";
        },
      }),
    });

    const id = mock.seed({ name: "echo", payload: { hello: 1 } });
    expect(await worker.processOnce()).toBe(true);
    expect(seen).toEqual([{ hello: 1 }]);
    expect(mock.find(id)?.status).toBe("succeeded");
  });

  test("processOnce returns false when nothing is eligible", async () => {
    const worker = new Worker({ client, workerId: "w1", registry: registry({}) });
    expect(await worker.processOnce()).toBe(false);
  });

  test("failing handler exhausts retries and dies", async () => {
    const worker = new Worker({
      client,
      workerId: "w1",
      baseBackoffMs: 1,
      maxBackoffMs: 2,
      registry: registry({
        flaky: () => {
          throw new Error("boom");
        },
      }),
    });

    const id = mock.seed({ name: "flaky", retries: 1 });
    await worker.processOnce();
    expect(mock.find(id)?.status).toBe("pending");
    expect(mock.find(id)?.attempts).toBe(1);

    await new Promise((r) => setTimeout(r, 10));
    await worker.processOnce();
    expect(mock.find(id)?.status).toBe("dead");
  });

  test("emits run/step lifecycle log lines", async () => {
    const writes: string[] = [];
    const originalWrite = process.stdout.write.bind(process.stdout);
    process.stdout.write = ((chunk: unknown): boolean => {
      writes.push(typeof chunk === "string" ? chunk : String(chunk));
      return true;
    }) as typeof process.stdout.write;

    try {
      const worker = new Worker({
        client,
        workerId: "w1",
        registry: registry({
          greet: async (_p, ctx) => {
            await ctx.run("fetch", () => "ok");
          },
        }),
      });
      mock.seed({ name: "greet" });
      await worker.processOnce();
    } finally {
      process.stdout.write = originalWrite;
    }

    const lines = writes
      .flatMap((c) => c.split("\n"))
      .filter((l) => l.length > 0)
      .map((l) => JSON.parse(l) as Record<string, unknown>);

    const find = (msg: string): Record<string, unknown> | undefined =>
      lines.find((l) => l["msg"] === msg);
    expect(find("Workflow started")).toMatchObject({ level: "info", scope: "worker:greet" });
    expect(find("Step completed")).toMatchObject({ level: "info", scope: "worker:greet" });
    expect(find("Step completed")!["fields"]).toMatchObject({ step: "fetch" });
    expect(find("Workflow completed")).toMatchObject({ level: "info", scope: "worker:greet" });
  });

  test("passes scoped loggers to workflow and step contexts", async () => {
    const writes: string[] = [];
    const originalWrite = process.stdout.write.bind(process.stdout);
    process.stdout.write = ((chunk: unknown): boolean => {
      writes.push(typeof chunk === "string" ? chunk : String(chunk));
      return true;
    }) as typeof process.stdout.write;

    let id = "";
    try {
      const worker = new Worker({
        client,
        workerId: "w1",
        registry: registry({
          cleanup: async (_p, ctx) => {
            expect(ctx.runId).toBe(id);
            expect(ctx.workflowName).toBe("cleanup");
            expect(ctx.attempt).toBe(1);
            ctx.logger.info("workflow cleanup");
            await ctx.run("delete-temp", (step) => {
              expect(step.runId).toBe(id);
              expect(step.workflowName).toBe("cleanup");
              expect(step.stepName).toBe("delete-temp");
              expect(step.attempt).toBe(1);
              step.logger.info("step cleanup");
              return "ok";
            });
          },
        }),
      });
      id = mock.seed({ name: "cleanup" });
      await worker.processOnce();
    } finally {
      process.stdout.write = originalWrite;
    }

    const lines = writes
      .flatMap((c) => c.split("\n"))
      .filter((l) => l.length > 0)
      .map((l) => JSON.parse(l) as Record<string, unknown>);

    const workflowLog = lines.find((l) => l["msg"] === "workflow cleanup");
    expect(workflowLog).toMatchObject({
      level: "info",
      scope: "cleanup",
      fields: { runId: id, workflow: "cleanup" },
    });

    const stepLog = lines.find((l) => l["msg"] === "step cleanup");
    expect(stepLog).toMatchObject({
      level: "info",
      scope: "cleanup:delete-temp",
      fields: { runId: id, workflow: "cleanup", step: "delete-temp" },
    });
  });

  test("emits Step cached on replay and Workflow cancelled on bail", async () => {
    const writes: string[] = [];
    const originalWrite = process.stdout.write.bind(process.stdout);
    process.stdout.write = ((chunk: unknown): boolean => {
      writes.push(typeof chunk === "string" ? chunk : String(chunk));
      return true;
    }) as typeof process.stdout.write;

    try {
      let pass = 0;
      const worker = new Worker({
        client,
        workerId: "w1",
        baseBackoffMs: 1,
        maxBackoffMs: 2,
        registry: registry({
          quit: async (_p, ctx) => {
            await ctx.run("prep", () => "v");
            pass += 1;
            if (pass === 1) throw new Error("retry");
            ctx.bail("done");
          },
        }),
      });
      mock.seed({ name: "quit", retries: 2 });
      await worker.processOnce();
      await new Promise((r) => setTimeout(r, 10));
      await worker.processOnce();
    } finally {
      process.stdout.write = originalWrite;
    }

    const lines = writes
      .flatMap((c) => c.split("\n"))
      .filter((l) => l.length > 0)
      .map((l) => JSON.parse(l) as Record<string, unknown>);
    const msgs = lines.map((l) => l["msg"]);
    expect(msgs).toContain("Step cached");
    expect(msgs).toContain("Workflow cancelled");
    const cancelled = lines.find((l) => l["msg"] === "Workflow cancelled");
    expect(cancelled).toMatchObject({ level: "info" });
    expect(cancelled!["fields"]).toMatchObject({ reason: "done" });
  });

  test("ctx.run memoizes across retries", async () => {
    const runs: Record<string, number> = { a: 0, b: 0 };
    let forceFail = true;
    const handler: WorkflowHandler = async (_payload, ctx) => {
      const v = await ctx.run("a", () => {
        runs.a += 1;
        return "user-1";
      });
      await ctx.run("b", () => {
        runs.b += 1;
        if (forceFail) throw new Error("fail-b");
        return v;
      });
    };

    const worker = new Worker({
      client,
      workerId: "w1",
      baseBackoffMs: 1,
      maxBackoffMs: 2,
      registry: registry({ multi: handler }),
    });

    const id = mock.seed({ name: "multi", retries: 4 });
    await worker.processOnce();
    expect(mock.find(id)?.status).toBe("pending");
    expect(mock.find(id)?.stepState).toEqual({ a: "user-1" });

    forceFail = false;
    await new Promise((r) => setTimeout(r, 10));
    await worker.processOnce();
    expect(mock.find(id)?.status).toBe("succeeded");
    expect(runs.a).toBe(1);
    expect(runs.b).toBe(2);
  });

  test("runLoop processes runs concurrently up to concurrency limit", async () => {
    const total = 5;
    let peak = 0;
    let inFlight = 0;
    let finished = 0;
    let resolveAllDone: () => void;
    const allDone = new Promise<void>((r) => (resolveAllDone = r));

    const worker = new Worker({
      client,
      workerId: "w1",
      concurrency: total,
      pollIntervalMs: 5,
      idleTimeoutMs: 500,
      registry: registry({
        slow: async () => {
          inFlight++;
          peak = Math.max(peak, inFlight);
          await new Promise((r) => setTimeout(r, 50));
          inFlight--;
          finished++;
          if (finished === total) resolveAllDone();
          return "ok";
        },
      }),
    });

    const ids: string[] = [];
    for (let i = 0; i < total; i++) {
      ids.push(mock.seed({ name: "slow", payload: { i } }));
    }

    worker.start();
    await allDone;
    await worker.drain();

    expect(peak).toBeGreaterThanOrEqual(2);
    for (const id of ids) {
      expect(mock.find(id)?.status).toBe("succeeded");
    }
  });
});
