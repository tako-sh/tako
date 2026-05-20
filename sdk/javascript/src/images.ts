const PUBLIC_IMAGE_BASE_PATH = "/_tako/image";
const DEFAULT_WIDTH = 1200;
const DEFAULT_QUALITY = 75;
const ALLOWED_WIDTH_VALUES = [320, 640, 960, 1200, 1920] as const;
const ALLOWED_WIDTHS = new Set<number>(ALLOWED_WIDTH_VALUES);
const ALLOWED_FORMATS = new Set(["webp", "avif"]);
const ALLOWED_LAYOUTS = new Set(["constrained", "full-width", "fixed"]);

export interface ImageUrlOptions {
  /** Output width. Must match one of the app's configured optimizer widths. */
  width?: number;
  /** Output quality, 1-100. Omitted when it matches Tako's default quality. */
  quality?: number;
  /** Output format override. Omit to use the optimizer's configured default. */
  format?: "avif" | "webp";
}

export type ImageSrcSetLayout = "constrained" | "full-width" | "fixed";

export interface ImageSrcSetOptions extends Omit<ImageUrlOptions, "width"> {
  /** Rendered image width, and fallback `src` width. */
  width: number;
  /** Responsive layout preset. Defaults to `constrained`, matching Astro's common responsive image mode. */
  layout?: ImageSrcSetLayout;
  /** Explicit generated widths. The fallback `width` is included automatically when omitted. */
  widths?: number[];
  /** Raw HTML sizes value. Omit to let Tako derive it from `layout` and `width`. */
  sizes?: string;
}

export interface ImageSrcSet {
  src: string;
  srcSet: string;
  sizes: string;
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

export function imageSrcSet(source: string, options: ImageSrcSetOptions): ImageSrcSet {
  const width = validateWidth(options.width);
  const layout = validateLayout(options.layout ?? "constrained");
  const sizes = validateSizes(options.sizes ?? defaultSizes(layout, width));
  const widths = responsiveWidths(width, layout, options.widths);
  const imageOptions = imageUrlOptionDefaults(options);

  return {
    src: imageUrl(source, { ...imageOptions, width }),
    srcSet: widths
      .map((candidateWidth) => {
        return `${imageUrl(source, { ...imageOptions, width: candidateWidth })} ${candidateWidth}w`;
      })
      .join(", "),
    sizes,
  };
}

function imageUrlOptionDefaults(options: ImageSrcSetOptions): Omit<ImageUrlOptions, "width"> {
  const imageOptions: Omit<ImageUrlOptions, "width"> = {};
  if (options.quality !== undefined) imageOptions.quality = options.quality;
  if (options.format !== undefined) imageOptions.format = options.format;
  return imageOptions;
}

function validateWidth(width: number): number {
  if (!Number.isInteger(width) || !ALLOWED_WIDTHS.has(width)) {
    throw new TypeError(
      `unsupported image width: ${width}. Use one of ${Array.from(ALLOWED_WIDTHS).join(", ")}`,
    );
  }
  return width;
}

function responsiveWidths(
  width: number,
  layout: ImageSrcSetLayout,
  explicitWidths: number[] | undefined,
): number[] {
  if (explicitWidths !== undefined) {
    if (explicitWidths.length === 0) {
      throw new TypeError("image widths must include at least one width");
    }
    return uniqueSortedWidths([...explicitWidths.map(validateWidth), width]);
  }

  const maxWidth = layout === "constrained" ? width * 2 : width;
  const candidates = ALLOWED_WIDTH_VALUES.filter((candidateWidth) => candidateWidth <= maxWidth);
  return uniqueSortedWidths([...candidates, width]);
}

function uniqueSortedWidths(widths: number[]): number[] {
  return Array.from(new Set(widths)).sort((a, b) => a - b);
}

function validateLayout(layout: ImageSrcSetLayout): ImageSrcSetLayout {
  if (!ALLOWED_LAYOUTS.has(layout)) {
    throw new TypeError(`unsupported image layout: ${String(layout)}`);
  }
  return layout;
}

function defaultSizes(layout: ImageSrcSetLayout, width: number): string {
  switch (layout) {
    case "constrained":
      return `(min-width: ${width}px) ${width}px, 100vw`;
    case "full-width":
      return "100vw";
    case "fixed":
      return `${width}px`;
  }
}

function validateSizes(sizes: string): string {
  if (sizes.trim() === "") {
    throw new TypeError("image sizes must be a non-empty string");
  }
  return sizes;
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
