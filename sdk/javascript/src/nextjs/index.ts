import path from "node:path";
import { fileURLToPath } from "node:url";

import { createNextjsAdapter } from "./adapter";
import type { NextConfigShape } from "./types";

const TAKO_IMAGE_WIDTHS = [320, 640, 960, 1200, 1920];
const IMAGE_LOADER_FILE = import.meta.url.endsWith(".ts")
  ? fileURLToPath(new URL("./image-loader.ts", import.meta.url))
  : fileURLToPath(new URL("./nextjs/image-loader.mjs", import.meta.url));

function imageLoaderFileForNext(): string {
  return path.relative(process.cwd(), IMAGE_LOADER_FILE);
}

export { createNextjsAdapter } from "./adapter";
export { createNextjsFetchHandler, shutdownManagedNextjsServers } from "./fetch-handler";
export type {
  NextAdapterContext,
  NextAdapterShape,
  NextBuildCompleteContext,
  NextConfigShape,
  NextjsBuildManifest,
  NextjsFetchHandlerOptions,
} from "./types";

/**
 * Wrap a Next.js config so it plays well with Tako.
 *
 * Forces `output: "standalone"` (required for Tako's deploy/runtime), sets
 * `adapterPath` to this module so Next uses the Tako adapter, and appends
 * `*.test` / `*.tako.test` to `allowedDevOrigins` so the dev proxy can hit
 * the Next dev server. It also configures `next/image` to use Tako's public
 * image optimizer globally.
 *
 * @typeParam T - The user's Next config type; preserved in the return type.
 * @param config - The Next.js config to augment.
 * @returns The augmented config with Tako-required fields applied.
 *
 * @example
 * ```typescript
 * // next.config.ts
 * import { withTako } from "tako.sh/nextjs";
 *
 * export default withTako({
 *   reactStrictMode: true,
 * });
 * ```
 */
export function withTako<T extends NextConfigShape>(config: T): T & NextConfigShape {
  return {
    ...config,
    output: "standalone",
    adapterPath: fileURLToPath(import.meta.url),
    allowedDevOrigins: [...(config.allowedDevOrigins ?? []), "*.test", "*.tako.test"],
    images: {
      ...config.images,
      loader: "custom",
      loaderFile: imageLoaderFileForNext(),
      deviceSizes: TAKO_IMAGE_WIDTHS,
      imageSizes: [],
    },
  };
}

export default createNextjsAdapter();
