import { defineWorkflow, workflow } from "tako.sh";

export default defineWorkflow("relay", {
  handler: async (input: { message: string }) => {
    return workflow("cw-e2e.broadcast").run(input);
  },
});
