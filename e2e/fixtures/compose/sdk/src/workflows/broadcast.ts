import { defineWorkflow } from "tako.sh";
import demo from "../channels/demo";

interface Payload {
  message: string;
}

// Enqueued by `web`, run by the `worker` service, published from the worker.
export default defineWorkflow("broadcast", {
  handler: async (payload: Payload) => {
    await demo.publish({ type: "message", data: { message: `worker:${payload.message}` } });
    return { delivered: payload.message };
  },
});
