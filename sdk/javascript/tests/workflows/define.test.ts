import { describe, expect, test } from "bun:test";
import {
  defineWorkflow,
  isWorkflowDefinition,
  isWorkflowExport,
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
