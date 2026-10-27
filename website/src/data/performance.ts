import results from "./benchmark-results.json";

export type BenchPoint = {
  conc: number;
  rps200: number;
  p99: number;
  non200: number;
  clientError: number;
  failure: number;
  cpu: number;
  proxyCpu: number;
  proxyPss: number;
  appPss: number;
};
export type Series = {
  id: string;
  label: string;
  color: string;
  dash?: boolean;
  points: BenchPoint[];
};
export type ChartUnit = "rps" | "ms" | "pct" | "mib";
export type ChartConfig = {
  title: string;
  description: string;
  key: keyof BenchPoint;
  max: number;
  ticks: number[];
  unit: ChartUnit;
  series: Series[];
};

export const benchmark = results;
export const reportUrl =
  "https://github.com/tako-sh/performance/blob/main/RESULTS.md";
export const format = (n: number | null, digits = 0) =>
  n === null
    ? "unavailable"
    : n.toLocaleString("en-US", { maximumFractionDigits: digits });
export const short = (n: number) =>
  n >= 1000 ? `${format(n / 1000, 1)}k` : format(n);
export const labels: Record<string, string> = {
  nginx: "nginx",
  haproxy: "HAProxy",
  tako: "Tako",
  envoy: "Envoy",
  caddy: "Caddy",
};
const colors: Record<string, string> = {
  nginx: "#8E879C",
  haproxy: "#B7A5D5",
  tako: "#9FD6C2",
  envoy: "#D8BC88",
  caddy: "#E88783",
};
export const proxyOrder = ["nginx", "haproxy", "tako", "envoy", "caddy"];
export type ResultRow = (typeof results.http)[number];
export function httpRow(proxy: string, concurrency: number): ResultRow {
  const row = results.http.find(
    (r) => r.proxy === proxy && r.conc === concurrency,
  );
  if (!row) throw new Error(`Missing benchmark row: ${proxy} c${concurrency}`);
  return row;
}
export function featureRow(endpoint: string, concurrency: number) {
  const row = results.features.find(
    (r) => r.endpoint === endpoint && r.conc === concurrency,
  );
  if (!row)
    throw new Error(
      `Missing feature benchmark row: ${endpoint} c${concurrency}`,
    );
  return row;
}
const point = (row: ResultRow): BenchPoint => ({
  conc: row.conc,
  rps200: row.metrics.rps200.mean,
  p99: row.metrics.p99.mean,
  non200: row.metrics.non200_pct.mean,
  clientError: row.metrics.client_error_pct.mean,
  failure: row.metrics.non200_pct.mean + row.metrics.client_error_pct.mean,
  cpu: row.metrics.cpu.mean,
  proxyCpu: row.metrics.proxyCpu.mean,
  proxyPss: row.metrics.memory.mean,
  appPss: row.metrics.appMemory.mean,
});
const proxySeries: Series[] = proxyOrder.map((id) => ({
  id,
  label: labels[id] ?? id,
  color: colors[id] ?? "#aaa",
  dash: id === "nginx",
  points: results.http
    .filter((r) => r.proxy === id)
    .sort((a, b) => a.conc - b.conc)
    .map(point),
}));
const featureSeries: Series[] = [
  { id: "channel-publish", label: "Channel publish", color: "#E88783" },
  { id: "workflow-enqueue", label: "Workflow enqueue", color: "#9FD6C2" },
].map(({ id, label, color }) => ({
  id,
  label,
  color,
  points: results.features
    .filter((r) => r.endpoint === id)
    .sort((a, b) => a.conc - b.conc)
    .map(point),
}));
function chart(
  title: string,
  description: string,
  key: keyof BenchPoint,
  unit: ChartUnit,
  series: Series[],
): ChartConfig {
  const high = Math.max(
    1,
    ...series.flatMap((s) => s.points.map((p) => p[key])),
  );
  const roughStep = high / 4;
  const power = 10 ** Math.floor(Math.log10(roughStep));
  const step =
    ([1, 2, 2.5, 5, 10].find((n) => n * power >= roughStep) ?? 10) * power;
  const max = step * Math.ceil(high / step);
  return {
    title,
    description,
    key,
    unit,
    series,
    max,
    ticks: Array.from({ length: Math.round(max / step) + 1 }, (_, i) =>
      Number((i * step).toPrecision(8)),
    ),
  };
}
export const httpCharts: ChartConfig[] = [
  chart(
    "HTTP 200 responses per second",
    "Mean of three runs. Only complete 200 responses count; elapsed time includes request drain.",
    "rps200",
    "rps",
    proxySeries,
  ),
  chart(
    "p99 response latency",
    "Mean of each run’s p99 time to response headers. Read alongside errors: failed requests do not contribute latency samples.",
    "p99",
    "ms",
    proxySeries,
  ),
  chart(
    "Non-200 responses and client errors",
    "Percentage of attempted outcomes that did not complete with HTTP 200. Lower is better; no failed runs are omitted.",
    "failure",
    "pct",
    proxySeries,
  ),
  chart(
    "Proxy memory under load",
    "Mean of each run’s peak process PSS, sampled during the same load as throughput. Includes all proxy workers; excludes app, load generator, and kernel socket memory.",
    "proxyPss",
    "mib",
    proxySeries,
  ),
  chart(
    "Proxy CPU usage",
    "Mean process CPU as a percentage of the shared two-CPU budget. 100% means both assigned CPUs are fully used.",
    "proxyCpu",
    "pct",
    proxySeries,
  ),
];
export const featureCharts: ChartConfig[] = [
  chart(
    "Channels and workflows 200 RPS",
    "The original JavaScript workload: publish a durable channel message or enqueue a workflow whose handler persists one ack step. This measures HTTP acceptance, not workflow completion throughput.",
    "rps200",
    "rps",
    featureSeries,
  ),
  chart(
    "Channels and workflows p99 latency",
    "Mean p99 time to response headers across three fresh runs at each concurrency.",
    "p99",
    "ms",
    featureSeries,
  ),
  chart(
    "Feature errors",
    "Non-200 responses plus client errors as a percentage of attempts. Every repetition is included.",
    "failure",
    "pct",
    featureSeries,
  ),
  chart(
    "Feature proxy memory",
    "Mean peak process PSS for the Tako server. Application and workflow workers are measured separately in the raw results.",
    "proxyPss",
    "mib",
    featureSeries,
  ),
];
export const performanceStructuredData = {
  "@context": "https://schema.org",
  "@type": "TechArticle",
  name: "Tako Performance Benchmarks",
  url: "https://tako.sh/performance/",
  dateModified: results.date,
  description:
    "A complete five-proxy HTTPS benchmark with throughput, latency, errors, CPU, memory, channel and workflow results from the same current test suite.",
};
