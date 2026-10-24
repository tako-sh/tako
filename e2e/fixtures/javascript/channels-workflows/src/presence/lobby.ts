import { definePresence } from "tako.sh";

export default definePresence("lobby", {
  auth: {
    verify: ({ header }) => (header?.value === "e2e" ? { subject: "e2e-user" } : false),
  },
});
