import { defineChannel } from "tako.sh";

interface Messages {
  message: { message: string };
}

export default defineChannel("demo", {
  auth: { verify: async () => true },
}).$messageTypes<Messages>();
