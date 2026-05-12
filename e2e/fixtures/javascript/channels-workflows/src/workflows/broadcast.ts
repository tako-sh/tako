import { defineWorkflow } from "tako.sh";
import demo from "../channels/demo";

interface Payload {
  message: string;
}

export default defineWorkflow<Payload>("broadcast", {
  handler: async (payload, ctx) => {
    await ctx.sleep("wait", 500);

    await demo.publish({ type: "message", data: { message: payload.message } });
  },
});
