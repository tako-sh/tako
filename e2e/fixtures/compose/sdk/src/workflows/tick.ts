import { defineWorkflow } from "tako.sh";

// A cron schedule registered by the worker of the current build.
export default defineWorkflow("tick", {
  schedule: "* * * * * *",
  handler: async () => Date.now(),
});
