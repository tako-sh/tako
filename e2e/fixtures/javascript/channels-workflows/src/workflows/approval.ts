import { defineWorkflow } from "tako.sh";

interface Payload {
  settleMs: number;
}

// Sleeps before it waits, so a signal sent right after enqueue arrives first
// and must be kept for the wait.
export default defineWorkflow<Payload, unknown>("approval", {
  handler: async (payload, ctx) => {
    await ctx.sleep("settle", payload.settleMs);
    return ctx.waitFor("decision", { timeout: 120_000 });
  },
});
