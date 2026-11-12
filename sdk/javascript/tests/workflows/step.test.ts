import { expect, test } from "bun:test";
import { Logger } from "../../src/logger";
import type { WorkflowsClient } from "../../src/workflows/rpc-client";
import { createStepAPI, DeferSignal, FailSignal, WaitSignal } from "../../src/workflows/step";
import type { StepState } from "../../src/workflows/types";

function checkpointFixture(state: StepState = {}, failSave = 1) {
  let saves = 0;
  const persisted: StepState = {};
  const client = {
    async saveStep(_run: string, _worker: string, name: string, value: unknown) {
      if (++saves === failSave) throw new Error("checkpoint unavailable");
      persisted[name] = value;
    },
  } as WorkflowsClient;
  const logger = new Logger("checkpoint-test");
  // Each call models one execution of the handler over the same saved state.
  const execution = () =>
    createStepAPI(client, "run", "worker", state, logger, (stepName) => ({
      runId: "run",
      workflowName: "test",
      stepName,
      attempt: 1,
      logger,
    }));
  return { api: execution(), execution, state, persisted, saves: () => saves };
}

async function reuseError(call: Promise<unknown>): Promise<string> {
  const signal = await call.then(
    () => undefined,
    (error: unknown) => error,
  );
  expect(signal).toBeInstanceOf(FailSignal);
  return (signal as FailSignal).error.message;
}

test("retrying a failed checkpoint cannot return an unpersisted cached result", async () => {
  const fixture = checkpointFixture();
  let executions = 0;
  const body = () => ++executions;
  await expect(fixture.api.run("charge", body)).rejects.toThrow("checkpoint unavailable");
  expect(fixture.state).not.toHaveProperty("charge");
  expect(await fixture.api.run("charge", body)).toBe(2);
  expect(fixture.persisted["charge"]).toBe(2);
  expect(await fixture.execution().run("charge", body)).toBe(2);
  expect(fixture.saves()).toBe(2);
  expect(executions).toBe(2);
});

test("a failed durable sleep marker is retried before deferring", async () => {
  const fixture = checkpointFixture();
  await expect(fixture.api.sleep("later", 60_000)).rejects.toThrow("checkpoint unavailable");
  expect(fixture.state).not.toHaveProperty("__sleep:later");
  await expect(fixture.api.sleep("later", 60_000)).rejects.toBeInstanceOf(DeferSignal);
  expect(fixture.persisted["__sleep:later"]).toEqual(fixture.state["__sleep:later"]);
  expect(fixture.saves()).toBe(2);
});

for (const hydrated of [false, true]) {
  test(`a failed sleep completion is retried (${hydrated ? "resumed" : "inline"})`, async () => {
    const fixture = checkpointFixture(
      hydrated ? { "__sleep:pause": { wakeAt: 0 } } : {},
      hydrated ? 1 : 2,
    );
    await expect(fixture.api.sleep("pause", 0)).rejects.toThrow("checkpoint unavailable");
    expect(fixture.state).not.toHaveProperty("pause");
    await fixture.api.sleep("pause", 0);
    expect(fixture.persisted["pause"]).toBe(true);
    const saves = fixture.saves();
    await fixture.execution().sleep("pause", 0);
    expect(fixture.saves()).toBe(saves);
  });
}

test("a step name reused within one execution fails instead of returning the first result", async () => {
  const fixture = checkpointFixture({}, 0);
  const processed: string[] = [];
  const process = (item: string) => () => {
    processed.push(item);
    return item;
  };
  expect(await fixture.api.run("process", process("first"))).toBe("first");
  const message = await reuseError(fixture.api.run("process", process("second")));
  expect(message).toContain('"process"');
  expect(message).toContain("unique within a run");
  expect(message).toContain('"process:1"');
  expect(processed).toEqual(["first"]);
});

test("a step replayed from an earlier execution returns its result once", async () => {
  const fixture = checkpointFixture({ charge: "receipt-1" }, 0);
  let executions = 0;
  const body = () => `receipt-${++executions + 1}`;
  expect(await fixture.api.run("charge", body)).toBe("receipt-1");
  await reuseError(fixture.api.run("charge", body));
  // The next execution replays the saved step again.
  expect(await fixture.execution().run("charge", body)).toBe("receipt-1");
  expect(executions).toBe(0);
  expect(fixture.saves()).toBe(0);
});

test("run, sleep, and waitFor share one step namespace", async () => {
  const cases: Array<[string, (api: ReturnType<typeof createStepAPI>) => Promise<unknown>]> = [
    ["run", (api) => api.run("gate", () => "again")],
    ["sleep", (api) => api.sleep("gate", 0)],
    ["waitFor", (api) => api.waitFor("gate")],
  ];
  for (const [first, use] of cases) {
    for (const [second, reuse] of cases) {
      // State saved by an earlier execution lets every kind of first use return.
      const saved = first === "sleep" ? { "__sleep:gate": { wakeAt: 0 }, gate: true } : { gate: 1 };
      const fixture = checkpointFixture(saved, 0);
      await use(fixture.api);
      const message = await reuseError(reuse(fixture.api));
      expect(message, `${first} then ${second}`).toContain('"gate"');
      expect(fixture.saves(), `${first} then ${second}`).toBe(0);
    }
  }
});

test("concurrent uses of one step name run the body once", async () => {
  const fixture = checkpointFixture({}, 0);
  let executions = 0;
  let release!: () => void;
  const gate = new Promise<void>((resolve) => (release = resolve));
  const first = fixture.api.run("charge", async () => {
    executions++;
    await gate;
    return "receipt";
  });
  await reuseError(fixture.api.run("charge", () => ++executions));
  release();
  expect(await first).toBe("receipt");
  expect(executions).toBe(1);
});

test("a wait that parks the run does not consume its name for the next execution", async () => {
  const fixture = checkpointFixture({}, 0);
  const parked = await fixture.api.waitFor("approval").then(
    () => undefined,
    (signal: unknown) => signal,
  );
  expect(parked).toBeInstanceOf(WaitSignal);
  fixture.state["approval"] = { approved: true };
  expect(await fixture.execution().waitFor("approval")).toEqual({ approved: true });
});
