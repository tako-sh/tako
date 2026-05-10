import { defineWorkflow } from "tako.sh";
import { cleanupOldRecords, RECORD_RETENTION_MS } from "../src/server/db";
import { logger } from "../src/tako.gen";

const cleanupLogger = logger.child("cleanup");

export default defineWorkflow<Record<string, never>>("cleanup", {
  schedule: "@daily",
  handler: async (_payload, step) => {
    const result = await step.run("delete-old-records", () => cleanupOldRecords());
    cleanupLogger.info("deleted old demo records", {
      retentionDays: RECORD_RETENTION_MS / (24 * 60 * 60 * 1000),
      requestsDeleted: result.requestsDeleted,
      basesDeleted: result.basesDeleted,
      cutoff: result.cutoff,
    });
  },
});
