import { createHmac } from "node:crypto";
import { isIP } from "node:net";
import { getImageSecret } from "./tako/secrets";

const IMAGE_BASE_PATH = "/_tako/image/v1";
const PRIVATE_MAX_AGE_SECONDS = 86_400;
const ALLOWED_WIDTHS = new Set([
  16, 32, 48, 64, 96, 128, 256, 384, 640, 750, 828, 1080, 1200, 1920, 2048, 3840,
]);

export interface CreateImageUrlOptions {
  width: number;
  quality?: number;
  public?: boolean;
  expiresInSeconds?: number;
  /**
   * Test hook for deterministic signatures. App code should omit this.
   * @internal
   */
  now?: Date;
}

export function createImageUrl(source: string, options: CreateImageUrlOptions): string {
  const secret = getImageSecret();
  if (!secret) {
    throw new Error("Tako image service is not available in this runtime.");
  }

  const width = validateWidth(options.width);
  const quality = validateQuality(options.quality ?? 75);
  validateSource(source);

  const visibility = options.public === true ? "public" : "private";
  const expires =
    visibility === "public"
      ? "-"
      : String(nowSeconds(options.now) + (options.expiresInSeconds ?? PRIVATE_MAX_AGE_SECONDS));
  const sig = signature(secret, visibility, width, quality, expires, source);
  const encodedSource = base64UrlEncode(source);

  return `${IMAGE_BASE_PATH}/${visibility}/${width}/${quality}/${expires}/${sig}/${encodedSource}`;
}

function validateWidth(width: number): number {
  if (!Number.isInteger(width) || !ALLOWED_WIDTHS.has(width)) {
    throw new Error(`unsupported image width: ${width}`);
  }
  return width;
}

function validateQuality(quality: number): number {
  if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
    throw new Error(`image quality must be an integer from 1 to 100`);
  }
  return quality;
}

function validateSource(source: string): void {
  if (
    source.length === 0 ||
    source.length > 2048 ||
    source.includes("\0") ||
    source.includes("\r") ||
    source.includes("\n") ||
    source.includes("#")
  ) {
    throw new Error("invalid image source");
  }

  if (source.startsWith("/")) {
    if (source.startsWith("//") || source.startsWith(IMAGE_BASE_PATH)) {
      throw new Error("invalid image source");
    }
    return;
  }

  let url: URL;
  try {
    url = new URL(source);
  } catch {
    throw new Error("invalid image source");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error("invalid image source");
  }
  if (url.username || url.password || url.hash) {
    throw new Error("invalid image source");
  }
  if (hostnameIsPrivateOrLocal(url.hostname)) {
    throw new Error("invalid image source");
  }
}

function nowSeconds(now: Date | undefined): number {
  return Math.floor((now?.getTime() ?? Date.now()) / 1000);
}

function signature(
  secret: string,
  visibility: string,
  width: number,
  quality: number,
  expires: string,
  source: string,
): string {
  return createHmac("sha256", secret)
    .update("v1\n")
    .update(visibility)
    .update("\n")
    .update(String(width))
    .update("\n")
    .update(String(quality))
    .update("\n")
    .update(expires)
    .update("\n")
    .update(source)
    .digest("base64url");
}

function base64UrlEncode(value: string): string {
  return Buffer.from(value, "utf8").toString("base64url");
}

function hostnameIsPrivateOrLocal(hostname: string): boolean {
  const host = stripIpv6Brackets(hostname).replace(/\.$/, "").toLowerCase();
  if (host.length === 0) return true;

  const ipVersion = isIP(host);
  if (ipVersion === 4) return ipv4IsPrivateOrLocal(host);
  if (ipVersion === 6) return ipv6IsPrivateOrLocal(host);

  return (
    !host.includes(".") ||
    host === "localhost" ||
    host.endsWith(".localhost") ||
    host === "local" ||
    host.endsWith(".local")
  );
}

function stripIpv6Brackets(hostname: string): string {
  return hostname.startsWith("[") && hostname.endsWith("]") ? hostname.slice(1, -1) : hostname;
}

function ipv4IsPrivateOrLocal(host: string): boolean {
  const octets = host.split(".").map((part) => Number(part));
  if (octets.length !== 4 || octets.some((part) => !Number.isInteger(part))) return true;
  const a = octets[0]!;
  const b = octets[1]!;
  const c = octets[2]!;
  const d = octets[3]!;
  return (
    a === 0 ||
    a === 10 ||
    a === 127 ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168) ||
    (a >= 224 && a <= 239) ||
    (a === 255 && b === 255 && c === 255 && d === 255)
  );
}

function ipv6IsPrivateOrLocal(host: string): boolean {
  return (
    host === "::" ||
    host === "::1" ||
    host.startsWith("fc") ||
    host.startsWith("fd") ||
    host.startsWith("fe8") ||
    host.startsWith("fe9") ||
    host.startsWith("fea") ||
    host.startsWith("feb") ||
    host.startsWith("ff")
  );
}
