import { defineConfig } from "tsdown";

const shared = {
  format: "esm" as const,
  dts: true,
  outDir: "dist",
  target: "esnext",
  deps: {
    onlyBundle: false as const,
    neverBundle: ["vite", "react", "react-dom"],
  },
};

export default defineConfig([
  {
    ...shared,
    platform: "node",
    clean: true,
    minify: false,
    entry: {
      index: "src/index.ts",
      runtime: "src/runtime.ts",
      vite: "src/vite.ts",
      internal: "src/internal.ts",
      nextjs: "src/nextjs/index.ts",
      "gen-channel-types": "bin/gen-channel-types.ts",
      "entrypoints/bun-server": "src/tako/entrypoints/bun-server.ts",
      "entrypoints/node-server": "src/tako/entrypoints/node-server.ts",
      "entrypoints/bun-worker": "src/tako/entrypoints/bun-worker.ts",
      "entrypoints/node-worker": "src/tako/entrypoints/node-worker.ts",
      "entrypoints/bun-dev": "src/tako/entrypoints/bun-dev.ts",
      "entrypoints/node-dev": "src/tako/entrypoints/node-dev.ts",
    },
  },
  {
    ...shared,
    platform: "browser",
    minify: true,
    entry: {
      client: "src/client.ts",
      react: "src/react.ts",
    },
  },
]);
