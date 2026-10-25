import { readdir } from "node:fs/promises";
import { join, parse } from "node:path";
import { pathToFileURL } from "node:url";
import { isStoreDefinition, type StoreDefinition } from "./define";

export interface DiscoveredStore {
  name: string;
  fileStem: string;
  store: StoreDefinition<unknown, unknown>;
}

/** Discover definitions without invoking loaders or authorization callbacks. */
export async function discoverStores(dir: string): Promise<DiscoveredStore[]> {
  const entries = await readdir(dir, { withFileTypes: true }).catch(
    (error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return [];
      throw error;
    },
  );
  const found: DiscoveredStore[] = [];
  const names = new Set<string>();
  for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
    if (entry.isDirectory())
      throw new Error(`nested store directory '${entry.name}' is not supported`);
    if (!entry.isFile()) continue;
    const file = parse(entry.name);
    if (!new Set([".ts", ".tsx", ".js", ".mjs", ".mts"]).has(file.ext) || /^[._]/.test(file.name))
      continue;
    const mod = (await import(
      /* @vite-ignore */ pathToFileURL(join(dir, entry.name)).href
    )) as Record<string, unknown>;
    const store = mod["default"];
    if (!isStoreDefinition(store))
      throw new Error(`store '${file.name}' must default-export a defineStore() result`);
    if (names.has(store.name)) throw new Error(`duplicate store '${store.name}' in ${entry.name}`);
    names.add(store.name);
    found.push({ name: store.name, fileStem: file.name, store });
  }
  return found;
}
