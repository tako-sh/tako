import { defineChannel } from "tako.sh";
import type { MissionChannelUpdate } from "../server/types";

export default defineChannel("mission-log", {
  paramsSchema: (t) => t.Object({ base: t.String({ minLength: 1 }) }),
}).$messageTypes<{
  update: MissionChannelUpdate;
}>();
