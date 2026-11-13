/**
 * The test workflows that the shared workflow scenarios in
 * `test-support/workflow-scenarios/` run. The Go runner registers the same
 * workflows under the same names with the same behavior; the scenarios'
 * README describes them.
 */

import { defineWorkflow } from "../../src/workflows/define";
import type { WorkflowExport } from "../../src/workflows/define";

export const scenarioWorkflows: Record<string, WorkflowExport> = {
  steps: defineWorkflow("steps", {
    handler: async (_payload, ctx) => {
      const a = await ctx.run("a", () => "a-done");
      const b = await ctx.run("b", () => 2);
      return { a, b };
    },
  }),
  fails: defineWorkflow("fails", {
    handler: () => {
      throw new Error("boom");
    },
  }),
  sleeps: defineWorkflow("sleeps", {
    handler: async (_payload, ctx) => {
      await ctx.sleep("nap", 3_600_000);
      return "awake";
    },
  }),
  wait: defineWorkflow("wait", {
    handler: (_payload, ctx) => ctx.waitFor("approval"),
  }),
  "wait-timeout": defineWorkflow("wait-timeout", {
    handler: (_payload, ctx) => ctx.waitFor("approval", { timeout: 600_000 }),
  }),
  duplicate: defineWorkflow("duplicate", {
    handler: async (_payload, ctx) => {
      await ctx.run("a", () => 1);
      await ctx.run("a", () => 2);
    },
  }),
  bail: defineWorkflow("bail", {
    handler: (_payload, ctx) => ctx.bail("not needed"),
  }),
  nightly: defineWorkflow("nightly", {
    schedule: "0 3 * * *",
    retries: 4,
    handler: () => {},
  }),
  hourly: defineWorkflow("hourly", {
    schedule: "0 * * * *",
    handler: () => {},
  }),
};
