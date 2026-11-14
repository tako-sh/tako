import { defineChannel } from "tako.sh";

export default defineChannel("shared", {
  transport: "ws",
  auth: { verify: ({ header }) => header?.value === "e2e" },
});
