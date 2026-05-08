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

function titleFontSize(title: string): number {
  if (title.length < 24) return 74;
  if (title.length < 40) return 68;
  if (title.length < 58) return 66;
  if (title.length < 76) return 56;
  if (title.length < 94) return 48;
  return 42;
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

function imageBackedCard(title: string, logoUri: string, imageUri: string) {
  return (
    <div
      style={{
        position: "relative",
        width: "100%",
        height: "100%",
        display: "flex",
        backgroundColor: "#18151F",
        color: "#FFF7ED",
        fontFamily: "Poppins",
        overflow: "hidden",
      }}
    >
      <img
        src={imageUri}
        width={1200}
        height={630}
        style={{
          position: "absolute",
          top: 0,
          left: 0,
          width: "1200px",
          height: "630px",
          objectFit: "cover",
        }}
      />
      <div
        style={{
          position: "absolute",
          top: 0,
          right: 0,
          bottom: 0,
          left: 0,
          background:
            "linear-gradient(90deg, rgba(18, 15, 23, 0.96) 0%, rgba(24, 21, 31, 0.88) 36%, rgba(24, 21, 31, 0.44) 63%, rgba(24, 21, 31, 0.12) 100%)",
        }}
      />
      <div
        style={{
          position: "absolute",
          top: 0,
          right: 0,
          bottom: 0,
          left: 0,
          background:
            "linear-gradient(180deg, rgba(18, 15, 23, 0.36) 0%, rgba(18, 15, 23, 0.04) 48%, rgba(18, 15, 23, 0.50) 100%)",
        }}
      />
      <div
        style={{
          position: "relative",
          width: "100%",
          height: "100%",
          display: "flex",
          flexDirection: "column",
          padding: "62px 76px 58px",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "15px", color: "#FFF0E6" }}>
          <img src={logoUri} width={44} height={44} />
          <div
            style={{
              fontFamily: "IBM Plex Mono",
              fontSize: "21px",
              fontWeight: 400,
              color: "#FFF0E6",
              letterSpacing: 0,
            }}
          >
            tako.sh/blog
          </div>
        </div>
        <div
          style={{
            display: "flex",
            flexGrow: 1,
            alignItems: "center",
            maxWidth: "700px",
            paddingBottom: "6px",
          }}
        >
          <div
            style={{
              fontSize: `${titleFontSize(title)}px`,
              fontWeight: 700,
              color: "#FFF7ED",
              lineHeight: 1.06,
              letterSpacing: 0,
              maxWidth: "700px",
            }}
          >
            {title}
          </div>
        </div>
      </div>
    </div>
  );
}

function plainCard(title: string, logoUri: string) {
  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        padding: "60px 72px 52px",
        backgroundColor: "#FFF9F4",
        fontFamily: "Poppins",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: "14px" }}>
        <img src={logoUri} width={44} height={44} />
        <div
          style={{
            fontFamily: "IBM Plex Mono",
            fontSize: "22px",
            fontWeight: 400,
            color: "#2F2A44",
            letterSpacing: 0,
          }}
        >
          tako.sh/blog
        </div>
      </div>
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          flexGrow: 1,
          justifyContent: "center",
        }}
      >
        <div
          style={{
            fontSize: `${titleFontSize(title)}px`,
            fontWeight: 700,
            color: "#2F2A44",
            lineHeight: 1.12,
            letterSpacing: 0,
            maxWidth: "980px",
          }}
        >
          {title}
        </div>
      </div>
    </div>
  );
}

async function compressPng(png: Uint8Array): Promise<Uint8Array> {
  const compressed = await sharp(png)
    .png({ compressionLevel: 9, adaptiveFiltering: true })
    .toBuffer();

  return compressed.byteLength < png.byteLength ? compressed : png;
}

export async function renderOgImage(title: string, imageSource?: string): Promise<Uint8Array> {
  const [poppinsBold, plexMono] = await Promise.all([
    fetchFont("Poppins", 700),
    fetchFont("IBM Plex Mono", 400),
  ]);

  const logoSvg = readFileSync(join(ROOT, "public/assets/logo.svg"), "utf-8");
  const logoUri = `data:image/svg+xml;base64,${Buffer.from(logoSvg).toString("base64")}`;
  const imageUri = sourceImageUri(imageSource);

  const svg = await satori(
    imageUri ? imageBackedCard(title, logoUri, imageUri) : plainCard(title, logoUri),
    {
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
