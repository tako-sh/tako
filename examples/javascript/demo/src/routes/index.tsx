import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { BASE_PRESETS, type BasePreset, type PlanetBase } from "../lib/bases";
import { parseHost } from "../lib/host";
import { demoImageUrl } from "../lib/images";
import { Landing } from "../components/landing";

type PageData = {
  rootOrigin: string;
  bases: PlanetBase[];
};

const getPageData = createServerFn().handler(async (): Promise<PageData> => {
  const { getRequest } = await import("@tanstack/react-start/server");
  const request = getRequest();
  const { rootOrigin } = parseHost(request?.headers.get("host") ?? "");
  return { rootOrigin, bases: BASE_PRESETS.map(toPlanetBase) };
});

export const Route = createFileRoute("/")({
  loader: () => getPageData(),
  component: Home,
});

function Home() {
  const { rootOrigin, bases } = Route.useLoaderData();
  return <Landing rootOrigin={rootOrigin} bases={bases} />;
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
