import { defineChannel } from "tako.sh";
import type { MissionChannelUpdate } from "../src/server/types";

export default defineChannel({
  paramsSchema: (t) => t.Object({ base: t.String({ minLength: 1 }) }),
}).$messageTypes<{
  update: MissionChannelUpdate;
}>();
