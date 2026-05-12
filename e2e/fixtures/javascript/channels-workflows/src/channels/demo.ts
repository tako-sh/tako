import { defineChannel } from "tako.sh";

interface Messages {
  message: { message: string };
}

export default defineChannel({
  name: "demo",
  auth: { verify: async () => true },
}).$messageTypes<Messages>();
