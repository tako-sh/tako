import { defineChannel } from "tako.sh";

export default defineChannel("shared", {
  transport: "ws",
  auth: { headerName: "X-API-Key", verify: ({ header }) => header?.value === "e2e" },
});
