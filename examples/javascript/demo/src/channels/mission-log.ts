import { defineChannel } from "tako.sh";
import type { MissionChannelUpdate } from "../server/types";

// `base` selects the stream: each base's page subscribes with
// `params: { base }`, and the server function and shipment workflow publish
// with `missionLog({ base })`, so a base only receives its own updates.
export default defineChannel("mission-log", {
  paramsSchema: (t) => t.Object({ base: t.String({ minLength: 1 }) }),
}).$messageTypes<{
  update: MissionChannelUpdate;
}>();
