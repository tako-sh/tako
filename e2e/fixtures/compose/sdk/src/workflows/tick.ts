import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { defineWorkflow, tako } from "tako.sh";

// A cron schedule registered by the worker of the current build.
export default defineWorkflow("tick", {
  schedule: "* * * * * *",
  handler: async () => {
    mkdirSync(tako.dataDir, { recursive: true });
    appendFileSync(join(tako.dataDir, "ticks.txt"), `${Date.now()}\n`);
  },
});
