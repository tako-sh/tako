import { createHmac } from "node:crypto";
import { isIP } from "node:net";
import { getImageSecret } from "./tako/secrets";

const IMAGE_BASE_PATH = "/_tako/image/v1";
const DEFAULT_PRIVATE_EXPIRATION_SECONDS = 604_800;
const DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE_SECONDS = 604_800;
const MAX_PRIVATE_BROWSER_CACHE_MAX_AGE_SECONDS = 31_536_000;
const DEFAULT_WIDTH = 1200;
const DEFAULT_QUALITY = 75;
const ALLOWED_WIDTHS = new Set([
  16, 32, 48, 64, 96, 128, 256, 384, 640, 750, 828, 1080, 1200, 1920, 2048, 3840,
]);

/**
 * Resize behavior when both `width` and `height` are set.
 *
 * - `"cover"` fills as much of the requested box as possible and crops overflow.
 * - `"contain"` fits inside the requested box and never crops.
 *
 * Tako never upscales source images.
 */
export type ImageFit = "cover" | "contain";

/**
 * Crop strategy for fixed-box cover images.
 *
 * `"smart"` uses libvips attention cropping. Omit `crop` to use centered
 * cropping, and omit both `fit` and `crop` for a heightless width resize.
 */
export type ImageCrop = "center" | "smart";

interface BaseImageUrlOptions {
  /**
   * Output quality from 1 to 100.
   *
   * @defaultValue 75
   */
  quality?: number;
  /**
   * Request a WebP fallback URL. Omit `format` for AVIF output.
   */
  format?: "webp";
}

type ImageResizeOptions =
  | {
      /**
       * Maximum output width. Defaults to `1200` when both `width` and `height` are omitted.
       *
       * Heightless output width is `min(width, originalWidth)`.
       */
      width?: number;
      height?: undefined;
      fit?: never;
      crop?: never;
    }
  | {
      /**
       * Maximum output width. Required whenever `height` is set.
       */
      width: number;
      /**
       * Maximum output height for fixed-box resizing.
       */
      height: number;
      /**
       * Fill as much of the requested box as possible and crop overflow.
       * The source is never upscaled.
       *
       * @defaultValue "cover"
       */
      fit?: "cover";
      /**
       * Crop strategy for cover resizing.
       *
       * @defaultValue "center"
       */
      crop?: ImageCrop;
    }
  | {
      /**
       * Maximum output width. Required whenever `height` is set.
       */
      width: number;
      /**
       * Maximum output height for fixed-box resizing.
       */
      height: number;
      /**
       * Fit inside the requested box without cropping or upscaling.
       */
      fit: "contain";
      crop?: never;
    };

type PrivateImageVisibilityOptions = {
  /**
   * Private image URLs are the default. Set `public: true` only for assets
   * that can be shared by public caches.
   *
   * @defaultValue false
   */
  public?: false;
  /**
   * Signed URL lifetime in seconds.
   *
   * @defaultValue 604800
   */
  expiresInSeconds?: number;
  /**
   * Browser-only cache max-age in seconds for private image responses.
   * This does not make private images cacheable by a CDN or shared cache.
   *
   * @defaultValue 604800
   */
  browserCacheMaxAgeSeconds?: number;
};

type PublicImageVisibilityOptions = {
  /**
   * Generate a stable public image URL with immutable public cache headers.
   * Public URLs do not include an expiration or private browser cache override.
   */
  public: true;
  expiresInSeconds?: never;
  browserCacheMaxAgeSeconds?: never;
};

/**
 * Options for a private image URL. Private URLs are the default and include a
 * signed expiration plus browser-only cache policy.
 */
export type PrivateImageUrlOptions = BaseImageUrlOptions &
  ImageResizeOptions &
  PrivateImageVisibilityOptions;

/**
 * Options for a public image URL. Public URLs are intended for non-user-specific
 * assets that may be stored by public caches.
 */
export type PublicImageUrlOptions = BaseImageUrlOptions &
  ImageResizeOptions &
  PublicImageVisibilityOptions;

/**
 * Options accepted by {@link createImageUrl}.
 *
 * Heightless requests default to maximum width `1200`. When `height` is set,
 * `width` is required, `fit` defaults to `"cover"`, and `crop` defaults to
 * `"center"`. Output dimensions never exceed the source dimensions.
 */
export type CreateImageUrlOptions = PrivateImageUrlOptions | PublicImageUrlOptions;

/**
 * Create a signed path-only URL for Tako's image optimizer.
 *
 * Omitted options produce a private AVIF URL with maximum width `1200`,
 * quality `75`, a 7-day signed expiration, and 7-day browser-only cache. Pass
 * `format: "webp"` only when a WebP fallback is needed.
 *
 * @param source - Local public path or remote HTTP(S) image URL to optimize.
 * @param opts - Optional resize, cache, visibility, and format controls.
 * @returns A signed URL under `/_tako/image/v1/<payload>.<signature>`.
 *
 * @example
 * ```ts
 * const avatar = createImageUrl("/avatars/u_123.png", { width: 256 });
 * const hero = createImageUrl("/assets/hero.jpg", { public: true });
 * ```
 */
export function createImageUrl(source: string, opts: CreateImageUrlOptions = {}): string {
  const secret = getImageSecret();
  if (!secret) {
    throw new Error("Tako image service is not available in this runtime.");
  }

  const resize = resizeFields(opts);
  const quality = validateQuality(opts.quality ?? DEFAULT_QUALITY);
  const format = validateFormat(opts.format);
  validateSource(source);

  const payload: ImagePayload = {
    ...(opts.public === true ? { pub: true } : {}),
    ...(format === "avif" ? {} : { f: format }),
    ...resize,
    ...(quality === DEFAULT_QUALITY ? {} : { q: quality }),
    ...(opts.public === true ? publicFields(opts) : privateFields(opts)),
    s: source,
  };
  const encodedPayload = base64UrlEncode(JSON.stringify(payload));
  const sig = signature(secret, encodedPayload);

  return `${IMAGE_BASE_PATH}/${encodedPayload}.${sig}`;
}

interface ImagePayload {
  pub?: true;
  f?: "webp";
  w?: number;
  h?: number;
  fit?: "contain";
  crop?: "smart";
  q?: number;
  c?: number;
  e?: number;
  s: string;
}

function publicFields(options: PublicImageUrlOptions): Record<string, never> {
  if (options.expiresInSeconds !== undefined || options.browserCacheMaxAgeSeconds !== undefined) {
    throw new Error("public image URLs cannot set browser cache or expiration options");
  }
  return {};
}

function privateFields(options: PrivateImageUrlOptions): Pick<ImagePayload, "c" | "e"> {
  const expiresInSeconds = validatePositiveSeconds(
    options.expiresInSeconds ?? DEFAULT_PRIVATE_EXPIRATION_SECONDS,
    "expiresInSeconds",
  );
  const browserCacheMaxAgeSeconds = validatePrivateBrowserCacheMaxAge(
    options.browserCacheMaxAgeSeconds ?? DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE_SECONDS,
  );
  return {
    ...(browserCacheMaxAgeSeconds === DEFAULT_PRIVATE_BROWSER_CACHE_MAX_AGE_SECONDS
      ? {}
      : { c: browserCacheMaxAgeSeconds }),
    e: nowSeconds() + expiresInSeconds,
  };
}

function resizeFields(options: ImageResizeOptions): Pick<ImagePayload, "w" | "h" | "fit" | "crop"> {
  if (options.height === undefined) {
    if (options.fit !== undefined || options.crop !== undefined) {
      throw new Error("image fit and crop require height");
    }
    const width = validateWidth(options.width ?? DEFAULT_WIDTH);
    return width === DEFAULT_WIDTH ? {} : { w: width };
  }

  if (options.width === undefined) {
    throw new Error("image height requires width");
  }
  const width = validateWidth(options.width);
  const height = validateDimension(options.height, "height");
  const fit = validateFit(options.fit ?? "cover");
  if (fit === "contain") {
    if (options.crop !== undefined) {
      throw new Error('image crop requires fit: "cover"');
    }
    return { w: width, h: height, fit };
  }

  const crop = validateCrop(options.crop ?? "center");
  return {
    w: width,
    h: height,
    ...(crop === "center" ? {} : { crop }),
  };
}

function validateWidth(width: number): number {
  return validateDimension(width, "width");
}

function validateDimension(value: number, name: "width" | "height"): number {
  if (!Number.isInteger(value) || !ALLOWED_WIDTHS.has(value)) {
    throw new Error(`unsupported image ${name}: ${value}`);
  }
  return value;
}

function validateQuality(quality: number): number {
  if (!Number.isInteger(quality) || quality < 1 || quality > 100) {
    throw new Error(`image quality must be an integer from 1 to 100`);
  }
  return quality;
}

function validateFormat(format: string | undefined): "avif" | "webp" {
  if (format === undefined) {
    return "avif";
  }
  if (format === "avif") {
    throw new Error("omit image format to use the default AVIF output");
  }
  if (format !== "webp") {
    throw new Error(`unsupported image format: ${format}`);
  }
  return "webp";
}

function validateFit(fit: string): ImageFit {
  if (fit !== "cover" && fit !== "contain") {
    throw new Error(`unsupported image fit: ${fit}`);
  }
  return fit;
}

function validateCrop(crop: string): ImageCrop {
  if (crop !== "center" && crop !== "smart") {
    throw new Error(`unsupported image crop: ${crop}`);
  }
  return crop;
}

function validatePositiveSeconds(seconds: number, fieldName: string): number {
  if (!Number.isInteger(seconds) || seconds < 1) {
    throw new Error(`${fieldName} must be a positive integer number of seconds`);
  }
  return seconds;
}

function validatePrivateBrowserCacheMaxAge(seconds: number): number {
  const value = validatePositiveSeconds(seconds, "browserCacheMaxAgeSeconds");
  if (value > MAX_PRIVATE_BROWSER_CACHE_MAX_AGE_SECONDS) {
    throw new Error(
      `browserCacheMaxAgeSeconds must be at most ${MAX_PRIVATE_BROWSER_CACHE_MAX_AGE_SECONDS}`,
    );
  }
  return value;
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

function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function signature(secret: string, encodedPayload: string): string {
  return createHmac("sha256", secret).update("v1\n").update(encodedPayload).digest("base64url");
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
