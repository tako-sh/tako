import { defineWorkflow } from "tako.sh";
import missionLog from "../channels/mission-log";
import { applyMissionEventToRequest } from "../src/server/db";
import type {
  MissionChannelUpdate,
  MissionLogEvent,
  Step,
  StepStatus,
  WorkflowStep,
} from "../src/server/types";
import { PIPELINE_STEPS } from "../src/server/types";

export type { MissionLogEvent, StepStatus, WorkflowStep };

export type OrderShipmentPayload = {
  requestId: string;
  base: string;
  item: string;
};

const STEP_TIMINGS: Record<Step, number> = {
  check: 1_500,
  pack: 2_500,
  load: 2_000,
  ship: 3_000,
  deliver: 2_500,
};

const MAX_ATTEMPTS = 3;
const FAIL_CHANCE = 0.45;
const RETRY_CAPABLE: ReadonlySet<Step> = new Set(["ship", "deliver"]);

function shouldStepFail(requestId: string, step: Step, attempt: number): boolean {
  if (!RETRY_CAPABLE.has(step)) return false;
  if (attempt >= MAX_ATTEMPTS) return false;
  const key = `${requestId}:${step}:${attempt}`;
  let hash = 2166136261;
  for (let i = 0; i < key.length; i++) {
    hash ^= key.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0) / 0xffffffff < FAIL_CHANCE;
}

export default defineWorkflow<OrderShipmentPayload>("order-shipment", {
  retries: MAX_ATTEMPTS - 1,
  handler: async (payload, ctx) => {
    const { requestId, base, item } = payload;
    const channel = missionLog({ base });

    async function emit(
      idSuffix: string,
      event: Omit<MissionLogEvent, "id" | "requestId" | "timestamp">,
    ) {
      const full: MissionLogEvent = {
        id: `${requestId}:${idSuffix}`,
        requestId,
        timestamp: Date.now(),
        ...event,
      };
      const request = applyMissionEventToRequest(full);
      if (!request) return;
      try {
        const update: MissionChannelUpdate = { request, event: full };
        await channel.publish({ type: "update", data: update });
      } catch (err) {
        ctx.logger.error("channel publish failed", { error: err, requestId, step: full.step });
      }
    }

    await ctx.run("received", () =>
      emit("received", {
        source: base,
        level: "info",
        message: `Request received: ${item}`,
      }),
    );

    for (const stepName of PIPELINE_STEPS) {
      await ctx.run(stepName, async () => {
        const attempt = ctx.attempt;
        await emit(`${stepName}-running-${attempt}`, {
          source: base,
          level: "info",
          message: labelFor(stepName, "running"),
          step: stepName,
          status: "running",
        });
        await new Promise((r) => setTimeout(r, STEP_TIMINGS[stepName]));

        if (shouldStepFail(requestId, stepName, attempt)) {
          await emit(`${stepName}-failed-${attempt}`, {
            source: "System",
            level: "warn",
            message: `${labelFor(stepName, "failed")} (attempt ${attempt}/${MAX_ATTEMPTS})`,
            step: stepName,
            status: "failed",
          });
          throw new Error(`${stepName} failed (attempt ${attempt})`);
        }

        await emit(`${stepName}-done-${attempt}`, {
          source: base,
          level: "info",
          message: labelFor(stepName, "done"),
          step: stepName,
          status: "done",
        });
      });
    }

    await ctx.run("complete", () =>
      emit("complete", {
        source: base,
        level: "info",
        message: `REQ-${shortId(requestId)} complete`,
        step: "complete",
        status: "done",
      }),
    );
  },
});

function shortId(requestId: string): string {
  return requestId.replace(/-/g, "").slice(0, 6).toUpperCase();
}

function labelFor(step: Step, status: StepStatus): string {
  const labels: Record<Step, { running: string; done: string; failed: string }> = {
    check: { running: "Checking order", done: "Order verified", failed: "Check failed" },
    pack: { running: "Packing items", done: "Items packed", failed: "Pack failed" },
    load: { running: "Loading carrier", done: "Carrier loaded", failed: "Load failed" },
    ship: { running: "Shipping", done: "In transit", failed: "Shipping failed, retrying" },
    deliver: {
      running: "Delivering",
      done: "Delivered",
      failed: "Delivery failed, retrying",
    },
  };
  return labels[step][status];
}
