import { Resvg } from "@resvg/resvg-js";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "fs";
import { dirname, extname, isAbsolute, join } from "path";
import React from "react";
import satori from "satori";
import sharp from "sharp";

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
      // Old UA asks Google Fonts for TTF, which satori can parse.
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

// JetBrains Mono is wide, so long titles step down faster than a proportional face would.
function titleFontSize(title: string): number {
  if (title.length < 28) return 56;
  if (title.length < 48) return 48;
  if (title.length < 68) return 42;
  if (title.length < 88) return 36;
  return 32;
}

function dataUri(path: string, mimeType: string): string {
  return `data:${mimeType};base64,${readFileSync(path).toString("base64")}`;
}

function blogImageUri(imageId: string): string {
  if (!/^[a-f0-9]{12}$/i.test(imageId)) {
    throw new Error(`Invalid blog image id: ${imageId}`);
  }

  const sourcePath = join(ROOT, "src/assets/blog", `${imageId}.png`);
  if (existsSync(sourcePath)) return dataUri(sourcePath, "image/png");
  throw new Error(`Blog image not found: src/assets/blog/${imageId}.png`);
}

function sourceImageUri(source: string | undefined): string | undefined {
  if (!source) return undefined;
  if (/^[a-f0-9]{12}$/i.test(source)) return blogImageUri(source);

  const imagePath = isAbsolute(source) ? source : join(process.cwd(), source);
  if (!existsSync(imagePath)) {
    throw new Error(`Image source not found: ${source}`);
  }

  const ext = extname(imagePath).toLowerCase();
  if (ext === ".png") return dataUri(imagePath, "image/png");
  if (ext === ".jpg" || ext === ".jpeg") return dataUri(imagePath, "image/jpeg");
  throw new Error(`Unsupported image source format: ${source}`);
}

const INK = "#1F1B2B";
const TEXT = "#EFEBE6";
const MUTED = "#B8B2C2";
const HAIRLINE = "rgba(239, 235, 230, 0.14)";

const CARD_WIDTH = 1200;
const ART_HEIGHT = 462;
const CAPTION_PADDING_X = 56;
const MARK_WIDTH = 200;

// Fit the title on two lines of JetBrains Mono (about 0.6em per character) beside the mark.
function captionTitleSize(title: string): number {
  const lineWidth = CARD_WIDTH - CAPTION_PADDING_X * 2 - MARK_WIDTH;
  const twoLineSize = Math.floor((lineWidth * 2 * 0.88) / (0.6 * title.length));
  return Math.max(28, Math.min(44, twoLineSize));
}

function mark(label: string, logoUri: string, logoSize: number) {
  return React.createElement(
    "div",
    { style: { display: "flex", alignItems: "center", gap: "12px", flexShrink: 0 } },
    React.createElement("img", { src: logoUri, width: logoSize, height: logoSize }),
    React.createElement(
      "div",
      { style: { fontSize: "18px", fontWeight: 400, color: MUTED } },
      label,
    ),
  );
}

// The post's painting across the top, its title on an ink caption strip below.
function imageBackedCard(title: string, label: string, logoUri: string, imageUri: string) {
  return React.createElement(
    "div",
    {
      style: {
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        backgroundColor: INK,
        fontFamily: "JetBrains Mono",
      },
    },
    React.createElement("img", {
      src: imageUri,
      width: CARD_WIDTH,
      height: ART_HEIGHT,
      style: { width: `${CARD_WIDTH}px`, height: `${ART_HEIGHT}px`, objectFit: "cover" },
    }),
    React.createElement(
      "div",
      {
        style: {
          flexGrow: 1,
          display: "flex",
          alignItems: "center",
          gap: "32px",
          padding: `0 ${CAPTION_PADDING_X}px`,
          borderTop: `1px solid ${HAIRLINE}`,
        },
      },
      React.createElement(
        "div",
        {
          style: {
            flexGrow: 1,
            flexShrink: 1,
            minWidth: 0,
            fontSize: `${captionTitleSize(title)}px`,
            fontWeight: 700,
            lineHeight: 1.18,
            letterSpacing: "-0.01em",
            color: TEXT,
          },
        },
        title,
      ),
      mark(label, logoUri, 32),
    ),
  );
}

// Fallback for a post without a painting.
function plainCard(title: string, label: string, logoUri: string) {
  return React.createElement(
    "div",
    {
      style: {
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        padding: "60px 64px 56px",
        backgroundColor: INK,
        fontFamily: "JetBrains Mono",
      },
    },
    mark(label, logoUri, 40),
    React.createElement(
      "div",
      { style: { display: "flex", flexGrow: 1, alignItems: "center" } },
      React.createElement(
        "div",
        {
          style: {
            fontSize: `${titleFontSize(title)}px`,
            fontWeight: 700,
            color: TEXT,
            lineHeight: 1.18,
            letterSpacing: "-0.01em",
          },
        },
        title,
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

export async function renderOgImage(title: string, imageSource?: string): Promise<Uint8Array> {
  const label = "tako.sh/blog";
  const [monoBold, monoRegular] = await Promise.all([
    fetchFont("JetBrains Mono", 700),
    fetchFont("JetBrains Mono", 400),
  ]);

  const logoSvg = readFileSync(join(ROOT, "public/assets/logo.svg"), "utf-8");
  const logoUri = `data:image/svg+xml;base64,${Buffer.from(logoSvg).toString("base64")}`;
  const imageUri = sourceImageUri(imageSource);

  const svg = await satori(
    imageUri ? imageBackedCard(title, label, logoUri, imageUri) : plainCard(title, label, logoUri),
    {
      width: 1200,
      height: 630,
      fonts: [
        { name: "JetBrains Mono", data: monoBold, weight: 700, style: "normal" },
        { name: "JetBrains Mono", data: monoRegular, weight: 400, style: "normal" },
      ],
    },
  );

  const resvg = new Resvg(svg, { fitTo: { mode: "width", value: 1200 } });
  return compressPng(resvg.render().asPng());
}

export async function generateOgImage(title: string, outputPath: string, imageSource?: string) {
  const png = await renderOgImage(title, imageSource);
  mkdirSync(dirname(outputPath), { recursive: true });
  writeFileSync(outputPath, png);
}
