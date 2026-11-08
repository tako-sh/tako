import { readdir } from "node:fs/promises";
import { join, parse } from "node:path";
import { pathToFileURL } from "node:url";
import type { NetworkHandler } from "../network-handlers";

export async function discoverNetworkHandlers(dir: string): Promise<Map<string, NetworkHandler>> {
  const found = new Map<string, NetworkHandler>();
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return found;
    throw error;
  }
  for (const entry of entries) {
    if (
      !entry.isFile() ||
      /^[._]/.test(entry.name) ||
      /\.d\.(?:ts|mts|cts)$/.test(entry.name) ||
      !/\.(?:ts|tsx|js|mjs|mts)$/.test(entry.name)
    )
      continue;
    const name = parse(entry.name).name;
    if (!/^[A-Za-z0-9_:-]{1,255}$/.test(name))
      throw new Error(`Invalid network handler name: ${name}`);
    const module = await import(/* @vite-ignore */ pathToFileURL(join(dir, entry.name)).href);
    if (typeof module.default !== "function")
      throw new Error(`Network handler '${name}' must default-export a function`);
    if (found.has(name)) throw new Error(`Duplicate network handler: ${name}`);
    found.set(name, module.default as NetworkHandler);
  }
  return found;
}
