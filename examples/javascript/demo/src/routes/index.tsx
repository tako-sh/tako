import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
import { imageUrl } from "tako.sh";
import { MissionController } from "@/components/mission-controller";
import { BASE_PRESETS, resolveBasePreset, type BasePreset, type PlanetBase } from "../lib/bases";
import { parseHost } from "../lib/host";
import type { BaseSnapshot } from "@/server/types";
import { Landing } from "../components/landing";

type PageData = {
  activeBase?: PlanetBase;
  baseSlug?: string;
  rootHost: string;
  rootOrigin: string;
  bases: PlanetBase[];
  snapshot?: BaseSnapshot;
};

const getPageData = createServerFn().handler(async (): Promise<PageData> => {
  const request = getRequest();
  const parsedHost = parseHost(request?.headers.get("host") ?? "");
  const bases = BASE_PRESETS.map(toPlanetBase);

  if (parsedHost.baseSlug) {
    const { getBaseSnapshot } = await import("@/server/db");
    return {
      activeBase: toPlanetBase(resolveBasePreset(parsedHost.baseSlug)),
      baseSlug: parsedHost.baseSlug,
      bases,
      rootHost: parsedHost.rootHost,
      rootOrigin: parsedHost.rootOrigin,
      snapshot: getBaseSnapshot(parsedHost.baseSlug),
    };
  }

  return {
    rootHost: parsedHost.rootHost,
    rootOrigin: parsedHost.rootOrigin,
    bases,
  };
});

export const Route = createFileRoute("/")({
  loader: () => getPageData(),
  component: Home,
});

function Home() {
  const { activeBase, baseSlug, rootHost, rootOrigin, bases, snapshot } = Route.useLoaderData();
  if (baseSlug && activeBase && snapshot) {
    return (
      <MissionController
        baseSlug={baseSlug}
        rootOrigin={rootOrigin}
        baseVisual={activeBase}
        initialSnapshot={snapshot}
      />
    );
  }

  return <Landing rootHost={rootHost} rootOrigin={rootOrigin} bases={bases} />;
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
