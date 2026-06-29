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

function titleFontSize(title: string): number {
  if (title.length < 16) return 82;
  if (title.length < 24) return 76;
  if (title.length < 34) return 68;
  return 60;
}

function chartBars(page: PageOgImage): number[] {
  const seed = Array.from(page.slug).reduce((total, char) => total + char.charCodeAt(0), 0);
  return [82, 118, 148, 102, 174, 134, 198].map(
    (height, index) => height - ((seed + index * 17) % 32),
  );
}

function pageCard(page: PageOgImage, logoUri: string) {
  const bars = chartBars(page);
  const barColors = ["#E88783", "#57B894", "#F2B94B", "#8D7AE6", "#E88783", "#57B894", "#F2B94B"];

  return React.createElement(
    "div",
    {
      style: {
        position: "relative",
        width: "100%",
        height: "100%",
        display: "flex",
        backgroundColor: "#FFF9F4",
        color: "#2F2A44",
        fontFamily: "Poppins",
        overflow: "hidden",
      },
    },
    React.createElement(
      "div",
      {
        style: {
          position: "absolute",
          top: 142,
          right: 58,
          width: 400,
          height: 318,
          display: "flex",
          flexDirection: "column",
          justifyContent: "flex-end",
          border: "3px solid #2F2A44",
          borderRadius: 16,
          backgroundColor: "#FFFFFF",
          padding: "34px 32px 31px",
          boxShadow: "14px 14px 0 #2F2A44",
        },
      },
      React.createElement(
        "div",
        {
          style: {
            display: "flex",
            alignItems: "flex-end",
            gap: 15,
            height: 250,
            borderBottom: "3px solid #2F2A44",
            padding: "0 4px",
          },
        },
        bars.map((height, index) =>
          React.createElement("div", {
            key: index,
            style: {
              width: 34,
              height,
              border: "3px solid #2F2A44",
              borderBottom: "0",
              borderRadius: "8px 8px 0 0",
              backgroundColor: barColors[index],
            },
          }),
        ),
      ),
    ),
    React.createElement(
      "div",
      {
        style: {
          position: "relative",
          width: 735,
          height: "100%",
          display: "flex",
          flexDirection: "column",
          padding: "58px 72px 58px",
        },
      },
      React.createElement(
        "div",
        { style: { display: "flex", alignItems: "center", gap: 15 } },
        React.createElement("img", { src: logoUri, width: 46, height: 46 }),
        React.createElement(
          "div",
          {
            style: {
              fontFamily: "IBM Plex Mono",
              fontSize: 21,
              fontWeight: 400,
              color: "#6F6683",
              letterSpacing: 0,
            },
          },
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
            paddingTop: 28,
          },
        },
        React.createElement(
          "div",
          {
            style: {
              fontSize: titleFontSize(page.title),
              fontWeight: 700,
              color: "#2F2A44",
              lineHeight: 1.05,
              letterSpacing: 0,
              maxWidth: 650,
            },
          },
          page.title,
        ),
        React.createElement(
          "div",
          {
            style: {
              marginTop: 28,
              maxWidth: 630,
              fontFamily: "Poppins",
              fontSize: 31,
              fontWeight: 700,
              lineHeight: 1.26,
              color: "#4B435F",
              letterSpacing: 0,
            },
          },
          page.description,
        ),
      ),
    ),
  );
}

async function compressPng(png: Uint8Array): Promise<Uint8Array> {
  const compressed = await sharp(png)
    .png({ compressionLevel: 9, adaptiveFiltering: true })
    .toBuffer();

  return compressed.byteLength < png.byteLength ? compressed : png;
}

export async function renderPageOgImage(page: PageOgImage): Promise<Uint8Array> {
  const [poppinsBold, plexMono] = await Promise.all([
    fetchFont("Poppins", 700),
    fetchFont("IBM Plex Mono", 400),
  ]);

  const logoSvg = readFileSync(join(ROOT, "public/assets/logo.svg"), "utf-8");
  const logoUri = `data:image/svg+xml;base64,${Buffer.from(logoSvg).toString("base64")}`;

  const svg = await satori(pageCard(page, logoUri), {
    width: 1200,
    height: 630,
    fonts: [
      { name: "Poppins", data: poppinsBold, weight: 700, style: "normal" },
      {
        name: "IBM Plex Mono",
        data: plexMono,
        weight: 400,
        style: "normal",
      },
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
