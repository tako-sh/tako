import { normalizeCanonicalPath } from "../utils/canonical";

export type PageOgImage = {
  path: string;
  slug: string;
  label: string;
  title: string;
  description: string;
};

export const pageOgImages = [
  {
    path: "/blog/",
    slug: "blog",
    label: "tako.sh/blog",
    title: "Tako Blog: Self-Hosted App Notes",
    description: "Updates, ideas, and progress from the Tako project.",
  },
  {
    path: "/performance/",
    slug: "performance",
    label: "tako.sh/performance",
    title: "Tako Performance Benchmarks for Self-Hosted Apps",
    description:
      "Benchmark results with raw proxy throughput, p99 latency, CPU, memory, channels, workflows, and methodology.",
  },
] satisfies PageOgImage[];

const pageOgByPath = new Map(pageOgImages.map((page) => [normalizeCanonicalPath(page.path), page]));

export function getPageOgImageForPath(pathname: string, site: URL | undefined): string | undefined {
  const page = pageOgByPath.get(normalizeCanonicalPath(pathname));
  if (!page) return undefined;
  return new URL(`/assets/og/${page.slug}.png`, site).href;
}
