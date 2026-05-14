import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
import { imageUrl, tako } from "tako.sh";
import { useChannel } from "tako.sh/react";
import { useState } from "react";
import { z } from "zod";
import missionLog from "../channels/mission-log";
import orderShipment from "../workflows/order-shipment";
import { BASE_PRESETS, resolveBasePreset, type BasePreset, type PlanetBase } from "../lib/bases";
import { parseHost } from "../lib/host";
import { Landing } from "../components/landing";
import { MissionControl } from "../components/mission-control";
import {
  EMPTY_RETRIES,
  EMPTY_STEPS,
  type InFlightRequest,
  type MissionLogEvent,
} from "../components/types";
import type { BaseSnapshot, DbSupplyRequest, MissionChannelUpdate } from "../server/types";
import { createRequest, getBaseSnapshot } from "@/server/db";

const EVENT_HISTORY_LIMIT = 80;
const REQUEST_HISTORY_LIMIT = 50;
const routeLogger = tako.logger.child("planetary-route");

const supplyRequestSchema = z.object({
  requestId: z.uuid(),
  base: z.string().min(1).max(64),
  item: z.string().min(1).max(120),
});

type SupplyRequestInput = z.infer<typeof supplyRequestSchema>;

type PageData = {
  tenantSlug: string | null;
  rootHost: string;
  rootOrigin: string;
  bases: PlanetBase[];
  activeBase: PlanetBase | null;
  snapshot: BaseSnapshot | null;
};

const getPageData = createServerFn().handler(async (): Promise<PageData> => {
  const request = getRequest();
  const { tenantSlug, rootHost, rootOrigin } = parseHost(request?.headers.get("host") ?? "");
  const bases = BASE_PRESETS.map(toPlanetBase);
  if (!tenantSlug) {
    return { tenantSlug: null, rootHost, rootOrigin, bases, activeBase: null, snapshot: null };
  }
  const snapshot = getBaseSnapshot(tenantSlug);
  const activeBase = toPlanetBase(resolveBasePreset(tenantSlug));
  return { tenantSlug, rootHost, rootOrigin, bases, activeBase, snapshot };
});

const enqueueSupplyRequest = createServerFn()
  .inputValidator((data) => supplyRequestSchema.parse(data))
  .handler(async ({ data }) => {
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

export const Route = createFileRoute("/")({
  loader: () => getPageData(),
  component: Home,
});

function Home() {
  const { tenantSlug, rootHost, rootOrigin, bases, activeBase, snapshot } = Route.useLoaderData();

  if (!tenantSlug || !snapshot) {
    return <Landing rootHost={rootHost} bases={bases} />;
  }

  return (
    <Controller
      tenantSlug={tenantSlug}
      rootOrigin={rootOrigin}
      baseVisual={activeBase}
      initialSnapshot={snapshot}
    />
  );
}

function Controller({
  tenantSlug,
  rootOrigin,
  baseVisual,
  initialSnapshot,
}: {
  tenantSlug: string;
  rootOrigin: string;
  baseVisual: PlanetBase | null;
  initialSnapshot: BaseSnapshot;
}) {
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
    params: { base: tenantSlug },
    onMessage,
  });
  const connected = status === "open";

  async function handleSubmit(payload: { item: string }) {
    if (submitting) return;
    const requestId = crypto.randomUUID();
    const input: SupplyRequestInput = {
      requestId,
      base: tenantSlug,
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
      tenantSlug={tenantSlug}
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

function toPlanetBase(base: BasePreset): PlanetBase {
  return {
    ...base,
    image: {
      hero: imageUrl(base.source, { width: 1200 }),
      card: imageUrl(base.source, { width: 640 }),
    },
  };
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
