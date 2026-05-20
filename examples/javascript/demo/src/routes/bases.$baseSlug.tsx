import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { imageUrl } from "tako.sh";
import { z } from "zod";
import { MissionController } from "@/components/mission-controller";
import { resolveBasePreset, type BasePreset, type PlanetBase } from "@/lib/bases";
import { parseHost } from "@/lib/host";
import type { BaseSnapshot } from "@/server/types";

const baseSlugSchema = z
  .string()
  .min(1)
  .max(64)
  .regex(/^[a-z0-9]+(?:-[a-z0-9]+)*$/);

type BasePageData = {
  baseSlug: string;
  rootOrigin: string;
  activeBase: PlanetBase;
  snapshot: BaseSnapshot;
};

const getBasePageData = createServerFn()
  .inputValidator((data) => baseSlugSchema.parse(data))
  .handler(async ({ data: baseSlug }): Promise<BasePageData> => {
    const { getRequest } = await import("@tanstack/react-start/server");
    const request = getRequest();
    const { getBaseSnapshot } = await import("@/server/db");
    const { rootOrigin } = parseHost(request?.headers.get("host") ?? "");
    return {
      baseSlug,
      rootOrigin,
      activeBase: toPlanetBase(resolveBasePreset(baseSlug)),
      snapshot: getBaseSnapshot(baseSlug),
    };
  });

export const Route = createFileRoute("/bases/$baseSlug")({
  loader: ({ params }) => getBasePageData({ data: params.baseSlug }),
  component: BasePage,
});

function BasePage() {
  const { baseSlug, rootOrigin, activeBase, snapshot } = Route.useLoaderData();
  return (
    <MissionController
      baseSlug={baseSlug}
      rootOrigin={rootOrigin}
      baseVisual={activeBase}
      initialSnapshot={snapshot}
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
