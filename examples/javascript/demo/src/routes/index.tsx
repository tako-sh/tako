import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { MissionController } from "@/components/mission-controller";
import { BASE_PRESETS, type BasePreset, type PlanetBase } from "../lib/bases";
import { parseHost, type ParsedHost } from "../lib/host";
import { demoImageUrl } from "../lib/images";
import type { BaseSnapshot } from "@/server/types";
import { Landing } from "../components/landing";

type PageData = {
  activeBase?: PlanetBase;
  baseSlug?: string;
  rootHost: string;
  rootOrigin: string;
  routeStyle: ParsedHost["routeStyle"];
  bases: PlanetBase[];
  snapshot?: BaseSnapshot;
};

const getPageData = createServerFn().handler(async (): Promise<PageData> => {
  const { getRequest } = await import("@tanstack/react-start/server");
  const request = getRequest();
  const parsedHost = parseHost(request?.headers.get("host") ?? "");
  const bases = BASE_PRESETS.map(toPlanetBase);

  if (parsedHost.baseSlug) {
    const { resolveBasePreset } = await import("../lib/bases");
    const { getBaseSnapshot } = await import("@/server/db");
    return {
      activeBase: toPlanetBase(resolveBasePreset(parsedHost.baseSlug)),
      baseSlug: parsedHost.baseSlug,
      bases,
      rootHost: parsedHost.rootHost,
      rootOrigin: parsedHost.rootOrigin,
      routeStyle: parsedHost.routeStyle,
      snapshot: getBaseSnapshot(parsedHost.baseSlug),
    };
  }

  return {
    rootHost: parsedHost.rootHost,
    rootOrigin: parsedHost.rootOrigin,
    routeStyle: parsedHost.routeStyle,
    bases,
  };
});

export const Route = createFileRoute("/")({
  loader: () => getPageData(),
  component: Home,
});

function Home() {
  const { activeBase, baseSlug, rootHost, rootOrigin, routeStyle, bases, snapshot } =
    Route.useLoaderData();
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

  return (
    <Landing rootHost={rootHost} rootOrigin={rootOrigin} routeStyle={routeStyle} bases={bases} />
  );
}

function toPlanetBase(base: BasePreset): PlanetBase {
  return {
    ...base,
    image: {
      hero: demoImageUrl(base.source, { width: 1200 }),
      card: demoImageUrl(base.source, { width: 640 }),
    },
  };
}
