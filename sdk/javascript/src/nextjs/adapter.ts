import { stageNextjsBuildOutput } from "./staging";
import type { NextAdapterShape } from "./types";

/**
 * Create the Next.js adapter object consumed by Next during build.
 *
 * Most apps should use {@link import("./index").withTako}; this lower-level
 * helper exists for Next adapter discovery and tests.
 */
export function createNextjsAdapter(): NextAdapterShape {
  return {
    name: "tako-nextjs",
    modifyConfig(config) {
      return {
        ...config,
        output: "standalone",
      };
    },
    async onBuildComplete({ projectDir, distDir }) {
      await stageNextjsBuildOutput(projectDir, distDir);
    },
  };
}
