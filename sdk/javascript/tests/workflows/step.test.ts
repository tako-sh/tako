import { expect, test } from "bun:test";
import { Logger } from "../../src/logger";
import type { WorkflowsClient } from "../../src/workflows/rpc-client";
import { createStepAPI, DeferSignal } from "../../src/workflows/step";
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
  const api = createStepAPI(client, "run", "worker", state, logger, (stepName) => ({
    runId: "run",
    workflowName: "test",
    stepName,
    attempt: 1,
    logger,
  }));
  return { api, state, persisted, saves: () => saves };
}

test("retrying a failed checkpoint cannot return an unpersisted cached result", async () => {
  const fixture = checkpointFixture();
  let executions = 0;
  const body = () => ++executions;
  await expect(fixture.api.run("charge", body)).rejects.toThrow("checkpoint unavailable");
  expect(fixture.state).not.toHaveProperty("charge");
  expect(await fixture.api.run("charge", body)).toBe(2);
  expect(fixture.persisted["charge"]).toBe(2);
  expect(await fixture.api.run("charge", body)).toBe(2);
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
    await fixture.api.sleep("pause", 0);
    expect(fixture.saves()).toBe(saves);
  });
}
