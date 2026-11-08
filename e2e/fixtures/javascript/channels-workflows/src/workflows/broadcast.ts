import { defineWorkflow } from "tako.sh";
import demo from "../channels/demo";

interface Payload {
  message: string;
}

export default defineWorkflow("broadcast", {
  handler: async (payload: Payload, ctx) => {
    await ctx.sleep("wait", 500);

    await demo.publish({ type: "message", data: { message: payload.message } });
    return { delivered: payload.message };
  },
});
