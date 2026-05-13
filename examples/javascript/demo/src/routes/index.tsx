import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
import { tako } from "tako.sh";
import type { CreateImageUrlOptions } from "tako.sh/server";
import { useChannel } from "tako.sh/react";
import { startTransition, useCallback, useMemo, useState } from "react";
import { z } from "zod";
import missionLog from "../channels/mission-log";
import orderShipment from "../workflows/order-shipment";
import {
  BASE_PRESETS,
  resolveBasePreset,
  type BaseImageUrls,
  type BasePreset,
  type PlanetBase,
} from "../lib/bases";
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
type ImageUrlSigner = (source: string, opts?: CreateImageUrlOptions) => string;

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

function parseHost(hostHeader: string): {
  tenantSlug: string | null;
  rootHost: string;
  rootOrigin: string;
} {
  const [hostPart, port] = hostHeader.split(":");
  const host = hostPart ?? "";
  const labels = host.split(".");
  const demoIndex = labels.indexOf("demo");
  if (demoIndex === -1) {
    const rootHost = host || "demo.tako.sh";
    return {
      tenantSlug: null,
      rootHost,
      rootOrigin: `//${port ? `${rootHost}:${port}` : rootHost}`,
    };
  }
  const rootHost = labels.slice(demoIndex).join(".");
  const tenantSlug = demoIndex === 1 ? (labels[0] ?? null) : null;
  return {
    tenantSlug,
    rootHost,
    rootOrigin: `//${port ? `${rootHost}:${port}` : rootHost}`,
  };
}

const getPageData = createServerFn().handler(async (): Promise<PageData> => {
  const { createImageUrl } = await import("tako.sh/server");
  const request = getRequest();
  const { tenantSlug, rootHost, rootOrigin } = parseHost(request?.headers.get("host") ?? "");
  const bases = BASE_PRESETS.map((base) => toPlanetBase(base, createImageUrl));
  if (!tenantSlug) {
    return { tenantSlug: null, rootHost, rootOrigin, bases, activeBase: null, snapshot: null };
  }
  const snapshot = getBaseSnapshot(tenantSlug);
  const activeBase = toPlanetBase(resolveBasePreset(tenantSlug), createImageUrl);
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
  const [requestsById, setRequestsById] = useState<Record<string, InFlightRequest>>(() =>
    indexRequests(initialSnapshot.requests),
  );
  const [events, setEvents] = useState<MissionLogEvent[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const requests = useMemo(
    () => Object.values(requestsById).sort((left, right) => right.createdAt - left.createdAt),
    [requestsById],
  );

  const onMessage = useCallback((raw: { type: string; data: unknown }) => {
    const msg = raw as { type: "update"; data: MissionChannelUpdate };
    const event = msg.data.event;
    startTransition(() => {
      setRequestsById((prev) => upsertRequest(prev, toInFlight(msg.data.request)));
      if (event) {
        setEvents((prev) => appendEvent(prev, event));
      }
    });
  }, []);

  const { status } = useChannel("mission-log", {
    params: { base: tenantSlug },
    onMessage,
  });
  const connected = status === "open";

  const handleSubmit = useCallback(
    async (payload: { item: string }) => {
      if (submitting) return;
      const requestId = crypto.randomUUID();
      const input: SupplyRequestInput = {
        requestId,
        base: tenantSlug,
        item: payload.item,
      };

      setRequestsById((prev) => upsertRequest(prev, optimisticRequest(input)));
      setSubmitError(null);
      setSubmitting(true);
      try {
        await enqueueSupplyRequest({ data: input });
      } catch (err) {
        routeLogger.error("supply request failed", { error: err, requestId });
        const message = err instanceof Error ? err.message : "unknown error";
        setSubmitError(`Request could not be enqueued: ${message}. Try again.`);
        setRequestsById((prev) => removeRequest(prev, requestId));
      } finally {
        setSubmitting(false);
      }
    },
    [submitting, tenantSlug],
  );

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

function toPlanetBase(base: BasePreset, signImageUrl: ImageUrlSigner): PlanetBase {
  return {
    ...base,
    image: createBaseImageUrls(base.source, signImageUrl),
  };
}

function createBaseImageUrls(source: string, signImageUrl: ImageUrlSigner): BaseImageUrls {
  return {
    source,
    hero: optimizedImageUrl(signImageUrl, source, {
      public: true,
      width: 1200,
      quality: 78,
    }),
    card: optimizedImageUrl(signImageUrl, source, {
      public: true,
      width: 640,
      quality: 76,
    }),
    thumb: optimizedImageUrl(signImageUrl, source, {
      public: true,
      width: 256,
      height: 256,
      crop: "smart",
      quality: 74,
    }),
  };
}

function optimizedImageUrl(
  signImageUrl: ImageUrlSigner,
  source: string,
  options: CreateImageUrlOptions,
): string {
  try {
    return signImageUrl(source, options);
  } catch (err) {
    if (err instanceof Error && err.message.includes("image service is not available")) {
      if (!tako.build) {
        return source;
      }
      throw new Error(`Image service did not sign ${source}. Run the demo through tako dev.`);
    }
    throw err;
  }
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

function indexRequests(rows: DbSupplyRequest[]): Record<string, InFlightRequest> {
  return rows.reduce<Record<string, InFlightRequest>>((acc, row) => {
    const request = toInFlight(row);
    acc[request.requestId] = request;
    return acc;
  }, {});
}

function upsertRequest(
  requests: Record<string, InFlightRequest>,
  incoming: InFlightRequest,
): Record<string, InFlightRequest> {
  const existing = requests[incoming.requestId];
  const next = {
    ...requests,
    [incoming.requestId]: existing
      ? {
          ...existing,
          ...incoming,
        }
      : incoming,
  };

  const requestIds = Object.keys(next);
  if (requestIds.length <= REQUEST_HISTORY_LIMIT) {
    return next;
  }

  const staleRequestId = requestIds
    .map((requestId) => next[requestId]!)
    .sort((left, right) => right.createdAt - left.createdAt)
    .slice(REQUEST_HISTORY_LIMIT)
    .map((request) => request.requestId)[0];
  if (!staleRequestId) {
    return next;
  }

  return removeRequest(next, staleRequestId);
}

function removeRequest(
  requests: Record<string, InFlightRequest>,
  requestId: string,
): Record<string, InFlightRequest> {
  if (!(requestId in requests)) {
    return requests;
  }
  const { [requestId]: _removed, ...rest } = requests;
  return rest;
}

function appendEvent(list: MissionLogEvent[], event: MissionLogEvent): MissionLogEvent[] {
  if (list.some((e) => e.id === event.id)) return list;
  return [event, ...list].slice(0, EVENT_HISTORY_LIMIT);
}
