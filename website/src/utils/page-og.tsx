import { Resvg } from "@resvg/resvg-js";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "fs";
import { dirname, join } from "path";
import React from "react";
import satori from "satori";
import sharp from "sharp";
import type { PageOgImage } from "../data/page-og";

const ROOT = process.cwd();
const FONT_CACHE = join(ROOT, "node_modules/.cache/fonts");

async function fetchFont(family: string, weight: number): Promise<ArrayBuffer> {
  const safe = family.replace(/\s+/g, "_");
  const cached = join(FONT_CACHE, `${safe}-${weight}.ttf`);
  if (existsSync(cached)) {
    const font = readFileSync(cached);
    return font.buffer.slice(font.byteOffset, font.byteOffset + font.byteLength) as ArrayBuffer;
  }

  const css = await fetch(
    `https://fonts.googleapis.com/css2?family=${encodeURIComponent(family)}:wght@${weight}&display=swap`,
    {
      headers: { "User-Agent": "Mozilla/4.0" },
    },
  ).then((r) => r.text());

  const url = css.match(/src: url\(([^)]+)\)/)?.[1];
  if (!url) throw new Error(`No font URL found for ${family}:${weight}`);

  const data = await fetch(url).then((r) => r.arrayBuffer());
  mkdirSync(FONT_CACHE, { recursive: true });
  writeFileSync(cached, Buffer.from(data));
  return data;
}

const INK = "#1F1B2B";
const RAISED = "#2F2A44";
const TEXT = "#EFEBE6";
const MUTED = "#B8B2C2";
const HAIRLINE = "rgba(239, 235, 230, 0.14)";

// Clean HTTPS 200 RPS from the public benchmark on /performance/.
const BENCHMARK = [
  { name: "Tako", value: 12.5, color: "#9BC4B6" },
  { name: "Caddy", value: 5.2, color: "#B8B2C2" },
  { name: "Envoy", value: 4.7, color: "#8E879C" },
];

function titleFontSize(title: string): number {
  if (title.length < 28) return 52;
  if (title.length < 44) return 44;
  return 38;
}

function benchmarkPanel() {
  const maxBar = 300;
  const maxValue = Math.max(...BENCHMARK.map((row) => row.value));
  return React.createElement(
    "div",
    {
      style: {
        width: 420,
        display: "flex",
        flexDirection: "column",
        gap: 22,
        padding: "34px 32px",
        border: `1px solid ${HAIRLINE}`,
        backgroundColor: RAISED,
      },
    },
    React.createElement(
      "div",
      { style: { fontSize: 18, fontWeight: 400, color: MUTED } },
      "clean HTTPS 200 RPS",
    ),
    ...BENCHMARK.map((row) =>
      React.createElement(
        "div",
        { key: row.name, style: { display: "flex", flexDirection: "column", gap: 8 } },
        React.createElement(
          "div",
          {
            style: { display: "flex", justifyContent: "space-between", fontSize: 20, color: TEXT },
          },
          React.createElement("div", { style: { fontWeight: 700 } }, row.name),
          React.createElement("div", { style: { fontWeight: 400 } }, `${row.value}k`),
        ),
        React.createElement("div", {
          style: {
            width: Math.round((row.value / maxValue) * maxBar),
            height: 12,
            backgroundColor: row.color,
          },
        }),
      ),
    ),
  );
}

function pageCard(page: PageOgImage, logoUri: string) {
  return React.createElement(
    "div",
    {
      style: {
        width: "100%",
        height: "100%",
        display: "flex",
        alignItems: "center",
        gap: 48,
        padding: "60px 64px 56px",
        backgroundColor: INK,
        color: TEXT,
        fontFamily: "JetBrains Mono",
      },
    },
    React.createElement(
      "div",
      // Yoga does not shrink flex items by default, so let the text column give way to the panel.
      {
        style: {
          flexGrow: 1,
          flexShrink: 1,
          minWidth: 0,
          height: "100%",
          display: "flex",
          flexDirection: "column",
        },
      },
      React.createElement(
        "div",
        { style: { display: "flex", alignItems: "center", gap: 14 } },
        React.createElement("img", { src: logoUri, width: 40, height: 40 }),
        React.createElement(
          "div",
          { style: { fontSize: 22, fontWeight: 400, color: MUTED } },
          page.label,
        ),
      ),
      React.createElement(
        "div",
        {
          style: {
            display: "flex",
            flexDirection: "column",
            justifyContent: "center",
            flexGrow: 1,
          },
        },
        React.createElement(
          "div",
          {
            style: {
              fontSize: titleFontSize(page.title),
              fontWeight: 700,
              lineHeight: 1.16,
              letterSpacing: "-0.01em",
            },
          },
          page.title,
        ),
        React.createElement(
          "div",
          {
            style: { marginTop: 24, fontSize: 24, fontWeight: 400, lineHeight: 1.45, color: MUTED },
          },
          page.description,
        ),
      ),
    ),
    page.slug === "performance" ? benchmarkPanel() : null,
  );
}

async function compressPng(png: Uint8Array): Promise<Uint8Array> {
  const compressed = await sharp(png)
    .png({ compressionLevel: 9, adaptiveFiltering: true })
    .toBuffer();

  return compressed.byteLength < png.byteLength ? compressed : png;
}

export async function renderPageOgImage(page: PageOgImage): Promise<Uint8Array> {
  const [monoBold, monoRegular] = await Promise.all([
    fetchFont("JetBrains Mono", 700),
    fetchFont("JetBrains Mono", 400),
  ]);

  const logoSvg = readFileSync(join(ROOT, "public/assets/logo.svg"), "utf-8");
  const logoUri = `data:image/svg+xml;base64,${Buffer.from(logoSvg).toString("base64")}`;

  const svg = await satori(pageCard(page, logoUri), {
    width: 1200,
    height: 630,
    fonts: [
      { name: "JetBrains Mono", data: monoBold, weight: 700, style: "normal" },
      { name: "JetBrains Mono", data: monoRegular, weight: 400, style: "normal" },
    ],
  });

  const resvg = new Resvg(svg, { fitTo: { mode: "width", value: 1200 } });
  return compressPng(resvg.render().asPng());
}

export async function generatePageOgImage(page: PageOgImage, outputPath: string) {
  const png = await renderPageOgImage(page);
  mkdirSync(dirname(outputPath), { recursive: true });
  writeFileSync(outputPath, png);
}
