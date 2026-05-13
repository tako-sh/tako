import { Resvg } from "@resvg/resvg-js";
import satori from "satori";

import { prettifyTenantSlug, sanitizeTenantSlug } from "@/lib/host";

const OXANIUM_WEIGHT = 700;
const PLEX_MONO_WEIGHT = 500;

let fontsPromise: Promise<{ oxanium: ArrayBuffer; plexMono: ArrayBuffer }> | null = null;

function loadFonts() {
  if (!fontsPromise) {
    fontsPromise = Promise.all([
      fetchGoogleFont("Oxanium", OXANIUM_WEIGHT),
      fetchGoogleFont("IBM Plex Mono", PLEX_MONO_WEIGHT),
    ]).then(([oxanium, plexMono]) => ({ oxanium, plexMono }));
    fontsPromise.catch(() => {
      fontsPromise = null;
    });
  }
  return fontsPromise;
}

async function fetchGoogleFont(family: string, weight: number): Promise<ArrayBuffer> {
  const cssUrl = `https://fonts.googleapis.com/css2?family=${encodeURIComponent(family)}:wght@${weight}&display=swap`;
  // Old UA forces Google Fonts to return a TTF — satori can't parse woff2.
  const css = await fetch(cssUrl, { headers: { "User-Agent": "Mozilla/4.0" } }).then((r) =>
    r.text(),
  );
  const match = css.match(/src: url\(([^)]+)\)/);
  const fontUrl = match?.[1];
  if (!fontUrl) {
    throw new Error(`Google Fonts returned no font URL for ${family}:${weight}`);
  }
  return await fetch(fontUrl).then((r) => r.arrayBuffer());
}

function titleFontSize(title: string): number {
  if (title.length < 14) return 128;
  if (title.length < 22) return 104;
  if (title.length < 32) return 84;
  return 68;
}

export interface OgInput {
  tenantSlug: string | null;
}

export async function renderOgPng({ tenantSlug }: OgInput): Promise<Uint8Array> {
  const { oxanium, plexMono } = await loadFonts();

  const slug = tenantSlug !== null ? sanitizeTenantSlug(tenantSlug) : null;
  const headline = slug ? prettifyTenantSlug(slug) : "Planetary Supply Desk";
  const eyebrow = slug ? "Planetary Supply Desk" : "A multi-tenant Tako demo";
  const footer = slug ? `${slug}.demo.tako.sh` : "demo.tako.sh";
  const statusLine = slug ? "online · mission control" : "";

  const svg = await satori(
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        justifyContent: "space-between",
        padding: "64px 72px",
        backgroundColor: "#121416",
        fontFamily: "Oxanium",
        position: "relative",
      }}
    >
      <div
        style={{
          position: "absolute",
          top: 0,
          left: 0,
          right: 0,
          height: "6px",
          background: "linear-gradient(90deg, #2DDBDE 0%, #2DDBDE 55%, #FFCE65 100%)",
        }}
      />
      <div
        style={{
          fontFamily: "IBM Plex Mono",
          fontSize: "22px",
          fontWeight: PLEX_MONO_WEIGHT,
          color: "#BAC9C9",
          letterSpacing: "0.04em",
          textTransform: "uppercase",
        }}
      >
        tako.sh · demo
      </div>
      <div style={{ display: "flex", flexDirection: "column", gap: "20px" }}>
        <div
          style={{
            fontFamily: "IBM Plex Mono",
            fontSize: "24px",
            fontWeight: PLEX_MONO_WEIGHT,
            color: "#FFCE65",
            letterSpacing: "0.06em",
            textTransform: "uppercase",
          }}
        >
          {eyebrow}
        </div>
        <div
          style={{
            fontSize: `${titleFontSize(headline)}px`,
            fontWeight: OXANIUM_WEIGHT,
            color: "#2DDBDE",
            lineHeight: 1.05,
            letterSpacing: "-0.02em",
            maxWidth: "1040px",
          }}
        >
          {headline}
        </div>
      </div>
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "flex-end",
          fontFamily: "IBM Plex Mono",
          fontSize: "22px",
          fontWeight: PLEX_MONO_WEIGHT,
          color: "#859493",
        }}
      >
        <div>{footer}</div>
        <div style={{ color: "#2DDBDE" }}>{statusLine}</div>
      </div>
    </div>,
    {
      width: 1200,
      height: 630,
      fonts: [
        { name: "Oxanium", data: oxanium, weight: OXANIUM_WEIGHT, style: "normal" },
        { name: "IBM Plex Mono", data: plexMono, weight: PLEX_MONO_WEIGHT, style: "normal" },
      ],
    },
  );

  const resvg = new Resvg(svg, { fitTo: { mode: "width", value: 1200 } });
  return resvg.render().asPng();
}
