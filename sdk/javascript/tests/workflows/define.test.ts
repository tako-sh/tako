import { describe, expect, test } from "bun:test";
import {
  defineWorkflow,
  isWorkflowDefinition,
  isWorkflowExport,
  setWorkflowRuntime,
  WORKFLOW_SYMBOL,
} from "../../src/workflows/define";

describe("WORKFLOW_SYMBOL", () => {
  test("is not equal to a separately created Symbol with the same description", () => {
    expect(Symbol("workflow")).not.toBe(WORKFLOW_SYMBOL);
  });
});

describe("defineWorkflow", () => {
  test("returns an export with enqueue + definition", () => {
    const fn = async () => {};
    const exp = defineWorkflow("my-job", { handler: fn, schedule: "0 9 * * *", local: true });
    expect(exp.definition.type).toBe(WORKFLOW_SYMBOL);
    expect(exp.definition.name).toBe("my-job");
    expect(exp.definition.handler).toBe(fn);
    expect(exp.definition.opts).toEqual({ schedule: "0 9 * * *", local: true });
    expect(typeof exp.enqueue).toBe("function");
  });

  test("enqueue stamps workflow retries unless the caller overrides", async () => {
    const calls: unknown[] = [];
    setWorkflowRuntime({
      enqueue: async (_name, _payload, options) => {
        calls.push(options);
        return "run-1";
      },
      signal: async () => 0,
      result: async () => ({ status: "succeeded", result: null, error: null }),
    });
    try {
      const exp = defineWorkflow("job", { retries: 4, handler: async () => {} });
      await exp.enqueue({});
      await exp.enqueue({}, { retries: 0 });
      expect(calls).toEqual([{ retries: 4 }, { retries: 0 }]);
    } finally {
      setWorkflowRuntime(null);
    }
  });

  test("opts only stores metadata outside the handler", () => {
    const fn = async () => {};
    const exp = defineWorkflow("my-job", { handler: fn });
    expect(exp.definition.handler).toBe(fn);
    expect(exp.definition.opts).toEqual({});
  });
});

describe("isWorkflowExport", () => {
  test("returns true for a defineWorkflow result", () => {
    const exp = defineWorkflow("j", { handler: async () => {} });
    expect(isWorkflowExport(exp)).toBe(true);
  });

  test("returns false for a plain function", () => {
    expect(isWorkflowExport(async () => {})).toBe(false);
  });

  test("returns false for null", () => {
    expect(isWorkflowExport(null)).toBe(false);
  });
});

describe("isWorkflowDefinition", () => {
  test("returns true for the inner definition of a defineWorkflow result", () => {
    const exp = defineWorkflow("j", { handler: async () => {} });
    expect(isWorkflowDefinition(exp.definition)).toBe(true);
  });

  test("returns false for a plain object with wrong type value", () => {
    expect(
      isWorkflowDefinition({
        type: Symbol("workflow"),
        name: "x",
        handler: () => {},
        opts: {},
      }),
    ).toBe(false);
  });

  test("returns false when required definition fields are missing", () => {
    expect(
      isWorkflowDefinition({
        type: WORKFLOW_SYMBOL,
        handler: () => {},
      }),
    ).toBe(false);
  });
});
