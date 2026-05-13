const PUBLIC_IMAGE_BASE_PATH = "/_tako/image";
const DEFAULT_WIDTH = 1200;
const DEFAULT_QUALITY = 75;
const ALLOWED_WIDTHS = new Set([320, 640, 960, 1200, 1920]);
const ALLOWED_FORMATS = new Set(["avif", "webp"]);

export interface ImageUrlOptions {
  /** Output width. Must match one of the app's configured optimizer widths. */
  width?: number;
  /** Output quality, 1-100. Omitted when it matches Tako's default quality. */
  quality?: number;
  /** Preferred output format. The optimizer still respects client support. */
  format?: "avif" | "webp";
}

export function imageUrl(source: string, options: ImageUrlOptions = {}): string {
  validatePublicSource(source);

  const width = validateWidth(options.width ?? DEFAULT_WIDTH);
  const quality = options.quality === undefined ? undefined : validateQuality(options.quality);
  const format = validateFormat(options.format);

  const params = new URLSearchParams();
  params.set("src", source);
  params.set("w", String(width));
  if (quality !== undefined && quality !== DEFAULT_QUALITY) {
    params.set("q", String(quality));
  }
  if (format !== undefined) {
    params.set("f", format);
  }

  return `${PUBLIC_IMAGE_BASE_PATH}?${params.toString()}`;
}

function validateWidth(width: number): number {
  if (!Number.isInteger(width) || !ALLOWED_WIDTHS.has(width)) {
    throw new TypeError(
      `unsupported image width: ${width}. Use one of ${Array.from(ALLOWED_WIDTHS).join(", ")}`,
    );
  }
  return width;
}

function validateQuality(quality: number): number {
  if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
    throw new TypeError("image quality must be an integer from 1 to 100");
  }
  return quality;
}

function validateFormat(format: ImageUrlOptions["format"]): ImageUrlOptions["format"] {
  if (format === undefined) return undefined;
  if (!ALLOWED_FORMATS.has(format)) {
    throw new TypeError(`unsupported image format: ${String(format)}`);
  }
  return format;
}

function validatePublicSource(source: string): void {
  if (!source) {
    throw new TypeError("invalid image source: expected a local path or http(s) URL");
  }

  if (source.startsWith("/")) {
    if (
      source.startsWith("//") ||
      source.startsWith("/_tako/image") ||
      source.includes("#") ||
      source.includes("\0")
    ) {
      throw new TypeError(`invalid image source: ${source}`);
    }
    return;
  }

  let url: URL;
  try {
    url = new URL(source);
  } catch {
    throw new TypeError(`invalid image source: ${source}`);
  }

  if (
    (url.protocol !== "http:" && url.protocol !== "https:") ||
    url.username ||
    url.password ||
    url.hash
  ) {
    throw new TypeError(`invalid image source: ${source}`);
  }
}
