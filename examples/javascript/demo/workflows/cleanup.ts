import { defineWorkflow } from "tako.sh";
import { cleanupOldRecords, RECORD_RETENTION_MS } from "../src/server/db";

export default defineWorkflow<Record<string, never>>("cleanup", {
  schedule: "@daily",
  handler: async (_payload, ctx) => {
    const result = await ctx.run("delete-old-records", () => cleanupOldRecords());
    ctx.logger.info("deleted old demo records", {
      retentionDays: RECORD_RETENTION_MS / (24 * 60 * 60 * 1000),
      requestsDeleted: result.requestsDeleted,
      basesDeleted: result.basesDeleted,
      cutoff: result.cutoff,
    });
  },
});
