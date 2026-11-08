import { expect, test } from "bun:test";
import { withWorkflowExecution, withinWorkflowStep } from "../../src/workflows/execution";
import {
  runWorkflow,
  waitForWorkflowResult,
  WorkflowResultError,
} from "../../src/workflows/result";
import { DeferSignal } from "../../src/workflows/step";
import type { WorkflowsClient } from "../../src/workflows/rpc-client";

function execution() {
  const history: Record<string, unknown> = {};
  const client = {
    appName: "parent/staging",
    async saveStep(_id: string, _owner: string, key: string, value: unknown) {
      history[key] = JSON.parse(JSON.stringify(value));
    },
  } as WorkflowsClient;
  return {
    history,
    run: <T>(fn: () => T) =>
      withWorkflowExecution(client, "parent-id", "owner", { ...history }, fn),
  };
}

test("lost enqueue acknowledgments and parent replay keep the same child and result", async () => {
  const context = execution();
  const ids: string[] = [];
  let status: "pending" | "succeeded" = "pending";
  let reads = 0;
  const call = () =>
    runWorkflow(
      "reports.generate",
      { order: 1 },
      async (_payload, opts) => {
        ids.push(opts.id!);
        if (ids.length === 1) throw new Error("lost acknowledgment");
        return opts.id!;
      },
      async () => {
        reads++;
        return { status, result: { url: "report.pdf" }, error: null };
      },
    );
  await expect(context.run(call)).rejects.toThrow("lost acknowledgment");
  try {
    await context.run(call);
    throw new Error("must defer");
  } catch (error) {
    expect(error).toBeInstanceOf(DeferSignal);
  }
  expect(ids).toHaveLength(2);
  expect(ids[0]).toBe(ids[1]);
  status = "succeeded";
  expect(await context.run(call)).toEqual({ url: "report.pdf" });
  const readCount = reads;
  expect(await context.run(call)).toEqual({ url: "report.pdf" });
  expect(reads).toBe(readCount);
  expect(ids).toHaveLength(2);
});

test("named step scopes and explicit keys keep child identities stable across skipped steps", async () => {
  const context = execution();
  const ids: string[] = [];
  const child = (step?: string) =>
    runWorkflow(
      "child",
      {},
      async (_payload, opts) => {
        ids.push(opts.id!);
        return opts.id!;
      },
      async () => ({ status: "succeeded", result: 1, error: null }),
      step ? { step } : {},
    );
  await context.run(async () => {
    await withinWorkflowStep("cached", () => child());
    await child("stable");
  });
  await context.run(() => child("stable"));
  expect(ids).toHaveLength(2);
  expect(ids[0]).not.toBe(ids[1]);
});

test("timeouts, missing runs and terminal failures are distinct and do not cancel the run", async () => {
  for (const status of ["dead", "cancelled", "missing", "timeout"] as const) {
    const read = async () =>
      status === "missing"
        ? null
        : {
            status: status === "timeout" ? ("pending" as const) : status,
            result: null,
            error: "reason",
          };
    try {
      await waitForWorkflowResult("child", "id", read, {
        timeout: status === "timeout" ? 0 : 1_000,
      });
      throw new Error("expected result error");
    } catch (error) {
      expect(error).toBeInstanceOf(WorkflowResultError);
      expect((error as WorkflowResultError).status).toBe(status);
    }
  }
});

test("durable calls reject changed payloads on replay", async () => {
  const context = execution();
  const call = (payload: unknown) =>
    runWorkflow(
      "child",
      payload,
      async () => "id",
      async () => ({ status: "succeeded", result: null, error: null }),
    );
  await context.run(() => call({ a: 1 }));
  await expect(context.run(() => call({ a: 2 }))).rejects.toThrow("changed during replay");
});

test("replay accepts JSON object keys reordered by storage", async () => {
  const context = execution();
  let enqueues = 0;
  const call = (payload: unknown) =>
    runWorkflow(
      "child",
      payload,
      async () => {
        enqueues++;
        return "id";
      },
      async () => ({ status: "succeeded", result: 42, error: null }),
    );
  expect(await context.run(() => call({ b: { z: 1, a: 2 }, a: 3 }))).toBe(42);
  expect(await context.run(() => call({ a: 3, b: { a: 2, z: 1 } }))).toBe(42);
  expect(enqueues).toBe(1);
});

test("a new parent with a reused run ID cannot reuse an older retained child", async () => {
  const ids: string[] = [];
  const call = () =>
    runWorkflow(
      "child",
      {},
      async (_payload, opts) => {
        ids.push(opts.id!);
        return opts.id!;
      },
      async () => ({ status: "succeeded", result: null, error: null }),
    );
  await execution().run(call);
  await execution().run(call);
  expect(ids[0]).not.toBe(ids[1]);
});

test("result deadlines survive deferral and also bound an in-flight read", async () => {
  const context = execution();
  const now = Date.now;
  let clock = now();
  let reads = 0;
  Date.now = () => clock;
  const wait = () =>
    waitForWorkflowResult(
      "child",
      "id",
      async () => {
        reads++;
        return { status: "pending", result: null, error: null };
      },
      { timeout: "1s" },
    );
  try {
    try {
      await context.run(wait);
    } catch (error) {
      expect(error).toBeInstanceOf(DeferSignal);
    }
    clock += 1_001;
    await expect(context.run(wait)).rejects.toMatchObject({ status: "timeout" });
    expect(reads).toBe(1);
  } finally {
    Date.now = now;
  }
  let release!: (value: null) => void;
  const pending = new Promise<null>((resolve) => {
    release = resolve;
  });
  try {
    await expect(
      waitForWorkflowResult("child", "id", () => pending, { timeout: 5 }),
    ).rejects.toMatchObject({ status: "timeout" });
  } finally {
    release(null);
  }
});
