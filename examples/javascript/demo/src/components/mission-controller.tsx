import { createServerFn } from "@tanstack/react-start";
import { useChannel } from "tako.sh/react";
import { useState } from "react";
import { z } from "zod";
import { MissionControl } from "@/components/mission-control";
import {
  EMPTY_RETRIES,
  EMPTY_STEPS,
  type InFlightRequest,
  type MissionLogEvent,
} from "@/components/types";
import type { PlanetBase } from "@/lib/bases";
import type { BaseSnapshot, DbSupplyRequest, MissionChannelUpdate } from "@/server/types";
import { tako } from "tako.sh";

const EVENT_HISTORY_LIMIT = 80;
const REQUEST_HISTORY_LIMIT = 50;
const routeLogger = tako.logger.child("planetary-route");

const supplyRequestSchema = z.object({
  requestId: z.uuid(),
  base: z.string().min(1).max(64),
  item: z.string().min(1).max(120),
});

type SupplyRequestInput = z.infer<typeof supplyRequestSchema>;

const enqueueSupplyRequest = createServerFn()
  .inputValidator((data) => supplyRequestSchema.parse(data))
  .handler(async ({ data }) => {
    const [{ createRequest }, { default: missionLog }, { default: orderShipment }] =
      await Promise.all([
        import("@/server/db"),
        import("@/channels/mission-log"),
        import("@/workflows/order-shipment"),
      ]);
    const request = createRequest({
      requestId: data.requestId,
      baseSlug: data.base,
      item: data.item,
    });

    try {
      await missionLog({ base: data.base }).publish({
        type: "update",
        data: { request },
      });
    } catch (err) {
      routeLogger.error("channel publish failed", { error: err });
    }
    await orderShipment.enqueue(data);
  });

type Props = {
  baseSlug: string;
  rootOrigin: string;
  baseVisual: PlanetBase | null;
  initialSnapshot: BaseSnapshot;
};

export function MissionController({ baseSlug, rootOrigin, baseVisual, initialSnapshot }: Props) {
  const [requests, setRequests] = useState<InFlightRequest[]>(() =>
    initialSnapshot.requests.map(toInFlight),
  );
  const [events, setEvents] = useState<MissionLogEvent[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  function onMessage(msg: { type: string; data: MissionChannelUpdate }) {
    if (msg.type !== "update") return;
    const event = msg.data.event;
    setRequests((prev) => upsertRequest(prev, toInFlight(msg.data.request)));
    if (event) {
      setEvents((prev) => appendEvent(prev, event));
    }
  }

  const { status } = useChannel("mission-log", {
    params: { base: baseSlug },
    onMessage,
  });
  const connected = status === "open";

  async function handleSubmit(payload: { item: string }) {
    if (submitting) return;
    const requestId = crypto.randomUUID();
    const input: SupplyRequestInput = {
      requestId,
      base: baseSlug,
      item: payload.item,
    };

    setRequests((prev) => upsertRequest(prev, optimisticRequest(input)));
    setSubmitError(null);
    setSubmitting(true);
    try {
      await enqueueSupplyRequest({ data: input });
    } catch (err) {
      routeLogger.error("supply request failed", { error: err, requestId });
      const message = err instanceof Error ? err.message : "unknown error";
      setSubmitError(`Request could not be enqueued: ${message}. Try again.`);
      setRequests((prev) => prev.filter((request) => request.requestId !== requestId));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <MissionControl
      baseSlug={baseSlug}
      rootOrigin={rootOrigin}
      baseVisual={baseVisual}
      inFlight={requests}
      events={events}
      submitting={submitting}
      connected={connected}
      submitError={submitError}
      onSubmit={handleSubmit}
    />
  );
}

function toInFlight(row: DbSupplyRequest): InFlightRequest {
  return {
    requestId: row.requestId,
    base: row.baseSlug,
    item: row.item,
    createdAt: row.createdAt,
    isComplete: row.isComplete,
    steps: row.steps,
    retries: row.retries,
  };
}

function optimisticRequest(input: SupplyRequestInput): InFlightRequest {
  return {
    requestId: input.requestId,
    base: input.base,
    item: input.item,
    createdAt: Date.now(),
    isComplete: false,
    steps: { ...EMPTY_STEPS },
    retries: { ...EMPTY_RETRIES },
  };
}

function upsertRequest(requests: InFlightRequest[], incoming: InFlightRequest): InFlightRequest[] {
  const found = requests.some((request) => request.requestId === incoming.requestId);
  const next = found
    ? requests.map((request) =>
        request.requestId === incoming.requestId ? { ...request, ...incoming } : request,
      )
    : [incoming, ...requests];

  return next
    .sort((left, right) => right.createdAt - left.createdAt)
    .slice(0, REQUEST_HISTORY_LIMIT);
}

function appendEvent(list: MissionLogEvent[], event: MissionLogEvent): MissionLogEvent[] {
  if (list.some((e) => e.id === event.id)) return list;
  return [event, ...list].slice(0, EVENT_HISTORY_LIMIT);
}
