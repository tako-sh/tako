export type BenchPoint = {
  conc: number;
  rps200: number;
  p99: number;
  non200: number;
  clientError: number;
  failure: number;
  cpu: number;
  proxyCpu: number;
  proxyRss: number;
  appRss: number;
};

export type Series = {
  id: string;
  label: string;
  color: string;
  points: BenchPoint[];
};

export type ChartUnit = "rps" | "ms" | "pct" | "mib";

export type ChartConfig = {
  title: string;
  eyebrow: string;
  description: string;
  key: keyof BenchPoint;
  max: number;
  ticks: number[];
  unit: ChartUnit;
  series: Series[];
};

export type SummaryMetric = {
  value: string;
  clean: boolean;
  detail?: string;
};

export type SummaryRow = {
  proxy: string;
  c5000: SummaryMetric;
  c10000: SummaryMetric;
  c20000: SummaryMetric;
  p99: SummaryMetric;
  note: string;
};

export type MemoryComparisonRow = {
  proxy: string;
  c5000: string;
  c10000: string;
  c20000: string;
  c20000Detail: string;
};

const withFailure = (points: Omit<BenchPoint, "failure">[]): BenchPoint[] =>
  points.map((point) => ({
    ...point,
    failure: Number((point.non200 + point.clientError).toFixed(2)),
  }));

const proxySeries: Series[] = [
  {
    id: "nginx",
    label: "nginx",
    color: "oklch(33% 0.03 285)",
    points: withFailure([
      {
        conc: 1000,
        rps200: 21404,
        p99: 100,
        non200: 0,
        clientError: 0,
        cpu: 98.6,
        proxyCpu: 31.1,
        proxyRss: 66,
        appRss: 39,
      },
      {
        conc: 2500,
        rps200: 18607,
        p99: 315,
        non200: 0,
        clientError: 0,
        cpu: 99.6,
        proxyCpu: 41.6,
        proxyRss: 114,
        appRss: 63,
      },
      {
        conc: 5000,
        rps200: 17698,
        p99: 1077,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 49.5,
        proxyRss: 220,
        appRss: 101,
      },
      {
        conc: 7500,
        rps200: 15834,
        p99: 1906,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 47.7,
        proxyRss: 216,
        appRss: 97,
      },
      {
        conc: 10000,
        rps200: 15309,
        p99: 1193,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 47.4,
        proxyRss: 221,
        appRss: 58,
      },
      {
        conc: 15000,
        rps200: 11384,
        p99: 6083,
        non200: 0.15,
        clientError: 0,
        cpu: 100,
        proxyCpu: 55.4,
        proxyRss: 389,
        appRss: 119,
      },
      {
        conc: 20000,
        rps200: 10991,
        p99: 3808,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 40.8,
        proxyRss: 262,
        appRss: 94,
      },
    ]),
  },
  {
    id: "haproxy",
    label: "HAProxy",
    color: "oklch(60% 0.09 168)",
    points: withFailure([
      {
        conc: 1000,
        rps200: 20848,
        p99: 103,
        non200: 0,
        clientError: 0,
        cpu: 99,
        proxyCpu: 36.2,
        proxyRss: 71,
        appRss: 40,
      },
      {
        conc: 2500,
        rps200: 18284,
        p99: 278,
        non200: 0,
        clientError: 0,
        cpu: 99.7,
        proxyCpu: 38,
        proxyRss: 140,
        appRss: 48,
      },
      {
        conc: 5000,
        rps200: 17050,
        p99: 1528,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 45.4,
        proxyRss: 265,
        appRss: 50,
      },
      {
        conc: 7500,
        rps200: 15666,
        p99: 3457,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 41.4,
        proxyRss: 275,
        appRss: 49,
      },
      {
        conc: 10000,
        rps200: 14788,
        p99: 6365,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 51.4,
        proxyRss: 476,
        appRss: 49,
      },
      {
        conc: 15000,
        rps200: 13176,
        p99: 12836,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 51.7,
        proxyRss: 580,
        appRss: 47,
      },
      {
        conc: 20000,
        rps200: 11162,
        p99: 15659,
        non200: 0,
        clientError: 0,
        cpu: 100,
        proxyCpu: 54.5,
        proxyRss: 896,
        appRss: 46,
      },
    ]),
  },
  {
    id: "tako",
    label: "Tako",
    color: "oklch(64% 0.14 22)",
    points: withFailure([
      {
        conc: 1000,
        rps200: 15223,
        p99: 154,
        non200: 0,
        clientError: 0,
        cpu: 99.3,
        proxyCpu: 46,
        proxyRss: 195,
        appRss: 36,
      },
      {
        conc: 2500,
        rps200: 13957,
        p99: 527,
        non200: 0,
        clientError: 0,
        cpu: 99.9,
        proxyCpu: 49.1,
        proxyRss: 387,
        appRss: 71,
      },
      {
        conc: 5000,
        rps200: 12504,
        p99: 2392,
        non200: 0,
        clientError: 0,
        cpu: 99.7,
        proxyCpu: 49.5,
        proxyRss: 748,
        appRss: 118,
      },
      {
        conc: 7500,
        rps200: 11543,
        p99: 5114,
        non200: 0,
        clientError: 0,
        cpu: 99.8,
        proxyCpu: 54.1,
        proxyRss: 998,
        appRss: 165,
      },
      {
        conc: 10000,
        rps200: 10373,
        p99: 7050,
        non200: 0,
        clientError: 0,
        cpu: 99.9,
        proxyCpu: 61.5,
        proxyRss: 1321,
        appRss: 217,
      },
      {
        conc: 15000,
        rps200: 8566,
        p99: 11648,
        non200: 0,
        clientError: 0,
        cpu: 99.9,
        proxyCpu: 60.5,
        proxyRss: 1964,
        appRss: 298,
      },
      {
        conc: 20000,
        rps200: 7266,
        p99: 15502,
        non200: 0,
        clientError: 0,
        cpu: 99.9,
        proxyCpu: 60.1,
        proxyRss: 2723,
        appRss: 392,
      },
    ]),
  },
  {
    id: "envoy",
    label: "Envoy",
    color: "oklch(64% 0.13 78)",
    points: withFailure([
      {
        conc: 1000,
        rps200: 12153,
        p99: 145,
        non200: 0,
        clientError: 0,
        cpu: 99.3,
        proxyCpu: 64.9,
        proxyRss: 125,
        appRss: 41,
      },
      {
        conc: 2500,
        rps200: 11651,
        p99: 374,
        non200: 0,
        clientError: 0,
        cpu: 99.8,
        proxyCpu: 63.2,
        proxyRss: 208,
        appRss: 85,
      },
      {
        conc: 5000,
        rps200: 4735,
        p99: 3104,
        non200: 0,
        clientError: 0.28,
        cpu: 99.3,
        proxyCpu: 67.9,
        proxyRss: 336,
        appRss: 133,
      },
      {
        conc: 7500,
        rps200: 4303,
        p99: 4843,
        non200: 0,
        clientError: 1.06,
        cpu: 99.7,
        proxyCpu: 67,
        proxyRss: 450,
        appRss: 133,
      },
      {
        conc: 10000,
        rps200: 3664,
        p99: 6660,
        non200: 2.09,
        clientError: 1.46,
        cpu: 100,
        proxyCpu: 66.1,
        proxyRss: 560,
        appRss: 133,
      },
      {
        conc: 15000,
        rps200: 3814,
        p99: 13665,
        non200: 39.19,
        clientError: 0,
        cpu: 99.9,
        proxyCpu: 92.2,
        proxyRss: 779,
        appRss: 132,
      },
      {
        conc: 20000,
        rps200: 828,
        p99: 26570,
        non200: 40.28,
        clientError: 1.02,
        cpu: 99.9,
        proxyCpu: 99.4,
        proxyRss: 999,
        appRss: 132,
      },
    ]),
  },
  {
    id: "caddy",
    label: "Caddy",
    color: "oklch(49% 0.08 296)",
    points: withFailure([
      {
        conc: 1000,
        rps200: 6599,
        p99: 260,
        non200: 0,
        clientError: 0,
        cpu: 98.5,
        proxyCpu: 52.4,
        proxyRss: 215,
        appRss: 46,
      },
      {
        conc: 2500,
        rps200: 5912,
        p99: 2313,
        non200: 0,
        clientError: 0,
        cpu: 98.6,
        proxyCpu: 64,
        proxyRss: 389,
        appRss: 98,
      },
      {
        conc: 5000,
        rps200: 5174,
        p99: 5198,
        non200: 0.14,
        clientError: 0,
        cpu: 99.2,
        proxyCpu: 70.5,
        proxyRss: 623,
        appRss: 132,
      },
      {
        conc: 7500,
        rps200: 4804,
        p99: 8871,
        non200: 0.39,
        clientError: 0,
        cpu: 99.9,
        proxyCpu: 71.7,
        proxyRss: 908,
        appRss: 135,
      },
      {
        conc: 10000,
        rps200: 1705,
        p99: 20281,
        non200: 0.06,
        clientError: 0,
        cpu: 99.7,
        proxyCpu: 80,
        proxyRss: 1235,
        appRss: 133,
      },
      {
        conc: 15000,
        rps200: 1715,
        p99: 23587,
        non200: 0,
        clientError: 4.75,
        cpu: 99.9,
        proxyCpu: 78.6,
        proxyRss: 1391,
        appRss: 127,
      },
      {
        conc: 20000,
        rps200: 1271,
        p99: 26386,
        non200: 0,
        clientError: 7.51,
        cpu: 100,
        proxyCpu: 76.1,
        proxyRss: 1534,
        appRss: 133,
      },
    ]),
  },
];

const featureSeries: Series[] = [
  {
    id: "channel",
    label: "Channel publish",
    color: "oklch(64% 0.14 22)",
    points: withFailure([
      {
        conc: 500,
        rps200: 7773,
        p99: 94,
        non200: 0,
        clientError: 0,
        cpu: 80.7,
        proxyCpu: 32.6,
        proxyRss: 142,
        appRss: 143,
      },
      {
        conc: 1000,
        rps200: 6589,
        p99: 213,
        non200: 0,
        clientError: 0,
        cpu: 83,
        proxyCpu: 51.2,
        proxyRss: 218,
        appRss: 225,
      },
      {
        conc: 2000,
        rps200: 6375,
        p99: 896,
        non200: 0,
        clientError: 0,
        cpu: 87.9,
        proxyCpu: 61.5,
        proxyRss: 373,
        appRss: 222,
      },
      {
        conc: 4000,
        rps200: 5818,
        p99: 3311,
        non200: 0,
        clientError: 0,
        cpu: 91.8,
        proxyCpu: 61.6,
        proxyRss: 679,
        appRss: 217,
      },
      {
        conc: 8000,
        rps200: 4595,
        p99: 6355,
        non200: 0,
        clientError: 0,
        cpu: 99.8,
        proxyCpu: 63.7,
        proxyRss: 1168,
        appRss: 348,
      },
    ]),
  },
  {
    id: "workflow",
    label: "Workflow enqueue",
    color: "oklch(60% 0.09 168)",
    points: withFailure([
      {
        conc: 500,
        rps200: 5555,
        p99: 126,
        non200: 0,
        clientError: 0,
        cpu: 79.5,
        proxyCpu: 29.5,
        proxyRss: 145,
        appRss: 207,
      },
      {
        conc: 1000,
        rps200: 5239,
        p99: 243,
        non200: 0,
        clientError: 0,
        cpu: 78,
        proxyCpu: 47.2,
        proxyRss: 222,
        appRss: 216,
      },
      {
        conc: 2000,
        rps200: 5049,
        p99: 1305,
        non200: 0,
        clientError: 0,
        cpu: 82.7,
        proxyCpu: 37.4,
        proxyRss: 357,
        appRss: 214,
      },
      {
        conc: 4000,
        rps200: 4709,
        p99: 3466,
        non200: 0,
        clientError: 0,
        cpu: 95.8,
        proxyCpu: 43.1,
        proxyRss: 636,
        appRss: 213,
      },
      {
        conc: 8000,
        rps200: 4001,
        p99: 7693,
        non200: 0,
        clientError: 0,
        cpu: 99.5,
        proxyCpu: 60.8,
        proxyRss: 1143,
        appRss: 287,
      },
    ]),
  },
];

const memoryPoint = (
  conc: number,
  memory: number,
  rps200: number,
  okPct: number,
  clientError: number,
): BenchPoint => ({
  conc,
  rps200,
  p99: 0,
  non200: Number((100 - okPct - clientError).toFixed(2)),
  clientError,
  failure: Number((100 - okPct).toFixed(2)),
  cpu: 0,
  proxyCpu: 0,
  proxyRss: memory,
  appRss: 0,
});

const memorySeries: Series[] = [
  {
    id: "nginx-memory",
    label: "nginx",
    color: "oklch(33% 0.03 285)",
    points: [
      memoryPoint(5000, 159, 17975, 100, 0),
      memoryPoint(10000, 159, 17141, 100, 0),
      memoryPoint(20000, 451, 10287, 99.43, 0),
    ],
  },
  {
    id: "haproxy-memory",
    label: "HAProxy",
    color: "oklch(60% 0.09 168)",
    points: [
      memoryPoint(5000, 248, 17901, 100, 0),
      memoryPoint(10000, 406, 15738, 100, 0),
      memoryPoint(20000, 624, 11905, 100, 0),
    ],
  },
  {
    id: "tako-memory",
    label: "Tako",
    color: "oklch(64% 0.14 22)",
    points: [
      memoryPoint(5000, 511, 12895, 100, 0),
      memoryPoint(10000, 911, 10979, 100, 0),
      memoryPoint(20000, 1700, 7818, 100, 0),
    ],
  },
  {
    id: "envoy-memory",
    label: "Envoy",
    color: "oklch(62% 0.12 78)",
    points: [
      memoryPoint(5000, 323, 4761, 99.76, 0.24),
      memoryPoint(10000, 554, 8566, 91.25, 0),
      memoryPoint(20000, 1004, 624, 51.41, 0.66),
    ],
  },
  {
    id: "caddy-memory",
    label: "Caddy",
    color: "oklch(49% 0.08 296)",
    points: [
      memoryPoint(5000, 621, 5258, 99.96, 0),
      memoryPoint(10000, 1213, 1822, 100, 0),
      memoryPoint(20000, 1511, 1610, 94.14, 5.86),
    ],
  },
];

export const httpCharts: ChartConfig[] = [
  {
    title: "HTTP 200 RPS by concurrency",
    eyebrow: "throughput",
    description:
      "Tako stays clean at high concurrency and beats Caddy and Envoy across the heavy rows. nginx and HAProxy show the static-proxy ceiling for this VM.",
    key: "rps200",
    max: 22000,
    ticks: [0, 5000, 10000, 15000, 20000],
    unit: "rps",
    series: proxySeries,
  },
  {
    title: "p99 latency by concurrency",
    eyebrow: "tail latency",
    description:
      "Tako completes every high-load row cleanly, with tail latency published beside RPS so the tradeoff stays visible. nginx is the tightest p99 reference in this run.",
    key: "p99",
    max: 27000,
    ticks: [0, 5000, 10000, 15000, 20000, 25000],
    unit: "ms",
    series: proxySeries,
  },
  {
    title: "Clean-run behavior by concurrency",
    eyebrow: "errors",
    description:
      "The line combines non-200 responses and client-side errors, so lower is better. Tako remains at 0% through c20000 on this run.",
    key: "failure",
    max: 45,
    ticks: [0, 10, 20, 30, 40],
    unit: "pct",
    series: proxySeries,
  },
  {
    title: "Memory by concurrency",
    eyebrow: "memory",
    description:
      "Memory is measured with process PSS, which avoids counting shared pages twice. At c20000, Caddy shows lower Memory than Tako, but Caddy also times out part of the load; compare the Memory line with the 200 labels below.",
    key: "proxyRss",
    max: 1800,
    ticks: [0, 450, 900, 1350, 1800],
    unit: "mib",
    series: memorySeries,
  },
];

export const featureCharts: ChartConfig[] = [
  {
    title: "Channels and workflows 200 RPS",
    eyebrow: "built-in features",
    description:
      "Both feature paths stay clean through c8000 on the same 2 vCPU VM, while still using the SDK, SQLite-backed persistence, and the proxy path.",
    key: "rps200",
    max: 8000,
    ticks: [0, 2000, 4000, 6000, 8000],
    unit: "rps",
    series: featureSeries,
  },
  {
    title: "Channels and workflows p99 latency",
    eyebrow: "feature tail latency",
    description:
      "Workflow enqueue persists steps, so it naturally carries more work than channel publish. Both paths stay clean through c8000 in this single-instance run.",
    key: "p99",
    max: 9000,
    ticks: [0, 3000, 6000, 9000],
    unit: "ms",
    series: featureSeries,
  },
];

export const heavyRows: SummaryRow[] = [
  {
    proxy: "nginx",
    c5000: { value: "17.7k", clean: true },
    c10000: { value: "15.3k", clean: true },
    c20000: { value: "11.0k", clean: true },
    p99: { value: "3.8s", clean: true },
    note: "Static-proxy RPS reference",
  },
  {
    proxy: "HAProxy",
    c5000: { value: "17.1k", clean: true },
    c10000: { value: "14.8k", clean: true },
    c20000: { value: "11.2k", clean: true },
    p99: { value: "15.7s", clean: true },
    note: "High RPS, wider p99",
  },
  {
    proxy: "Tako",
    c5000: { value: "12.5k", clean: true },
    c10000: { value: "10.4k", clean: true },
    c20000: { value: "7.3k", clean: true },
    p99: { value: "15.5s", clean: true },
    note: "Clean through c20000",
  },
  {
    proxy: "Envoy",
    c5000: { value: "4.7k", clean: false, detail: "99.72% 200" },
    c10000: { value: "3.7k", clean: false, detail: "96.45% 200" },
    c20000: { value: "0.8k", clean: false, detail: "58.70% 200" },
    p99: { value: "26.6s", clean: false, detail: "58.70% 200" },
    note: "58.70% 200 at c20000",
  },
  {
    proxy: "Caddy",
    c5000: { value: "5.2k", clean: false, detail: "99.86% 200" },
    c10000: { value: "1.7k", clean: false, detail: "99.94% 200" },
    c20000: { value: "1.3k", clean: false, detail: "92.49% 200" },
    p99: { value: "26.4s", clean: false, detail: "92.49% 200" },
    note: "92.49% 200 at c20000",
  },
];

export const memoryRows: MemoryComparisonRow[] = [
  {
    proxy: "nginx",
    c5000: "159 MiB",
    c10000: "159 MiB",
    c20000: "451 MiB",
    c20000Detail: "99.43% 200",
  },
  {
    proxy: "HAProxy",
    c5000: "248 MiB",
    c10000: "406 MiB",
    c20000: "624 MiB",
    c20000Detail: "100% 200",
  },
  {
    proxy: "Tako",
    c5000: "511 MiB",
    c10000: "911 MiB",
    c20000: "1.7 GiB",
    c20000Detail: "100% 200",
  },
  {
    proxy: "Envoy",
    c5000: "323 MiB",
    c10000: "554 MiB",
    c20000: "1.0 GiB",
    c20000Detail: "51.41% 200",
  },
  {
    proxy: "Caddy",
    c5000: "621 MiB",
    c10000: "1.2 GiB",
    c20000: "1.5 GiB",
    c20000Detail: "94.14% 200",
  },
];

export const performanceStructuredData = {
  "@context": "https://schema.org",
  "@type": "WebPage",
  name: "Tako Performance Benchmarks for Self-Hosted Apps",
  url: "https://tako.sh/performance/",
  description:
    "Single-VM Tako performance results with proxy, CPU, memory, clean-run, channel, and workflow benchmark graphs.",
};
