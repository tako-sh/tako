#!/usr/bin/env bun

import { resolve } from "node:path";
import { generateNetworkTypes } from "../src/network/generate";

export { generateNetworkTypes };

if (import.meta.main) {
  const [root, declarationDir, app] = Bun.argv.slice(2);
  if (!root || !declarationDir || !app) {
    throw new Error("usage: tako-sh-gen-network-types <app-root> <declaration-dir> <app-name>");
  }
  process.stdout.write(
    JSON.stringify(
      await generateNetworkTypes(
        resolve(root),
        resolve(declarationDir),
        app,
        JSON.parse(await Bun.stdin.text()).imports,
      ),
    ),
  );
}
