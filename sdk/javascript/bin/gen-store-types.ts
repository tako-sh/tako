#!/usr/bin/env bun

import { dirname, relative, resolve, sep } from "node:path";
import { discoverStores } from "../src/stores/discovery";

/** Generate Store metadata without running any Store's loader. */
export async function generateStoreTypes(
  storesDir: string,
  declarationDir: string = dirname(storesDir),
): Promise<string> {
  const discovered = await discoverStores(storesDir);
  const relativeDir = relative(declarationDir, storesDir).split(sep).join("/");
  const importDir = relativeDir.startsWith(".") ? relativeDir : `./${relativeDir}`;
  return [
    "/**",
    " * Project-specific Store metadata discovered from `<app_root>/stores/`.",
    " * Used by `tako.sh/client` and `tako.sh/react` to type Store params and data.",
    " */",
    "export interface TakoStores {",
    ...discovered.map(
      ({ name, fileStem }) =>
        `  ${JSON.stringify(name)}: import("tako.sh").InferStore<typeof import(${JSON.stringify(`${importDir}/${fileStem}`)}).default>;`,
    ),
    "}",
    "",
  ].join("\n");
}

if (import.meta.main) {
  const storesDir = Bun.argv[2];
  if (!storesDir) {
    console.error("usage: tako-sh-gen-store-types <stores-dir> [declaration-dir]");
    process.exit(2);
  }
  const declarationDir = Bun.argv[3];
  process.stdout.write(
    await generateStoreTypes(
      resolve(storesDir),
      declarationDir === undefined ? undefined : resolve(declarationDir),
    ),
  );
}
