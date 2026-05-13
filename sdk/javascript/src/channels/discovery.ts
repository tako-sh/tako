import { readdir, stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { join, parse } from "node:path";
import { isChannelDefinition, isChannelExport, type ChannelDefinition } from "./meta";

const VALID_EXTS = new Set([".ts", ".tsx", ".js", ".mjs", ".mts"]);

export interface DiscoveredChannel {
  name: string;
  fileStem: string;
  definition: ChannelDefinition;
}

export async function discoverChannels(dir: string): Promise<DiscoveredChannel[]> {
  if (!(await dirExists(dir))) return [];

  const entries = await readdir(dir, { withFileTypes: true });
  const found: DiscoveredChannel[] = [];
  const seenNames = new Map<string, string>();

  for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
    if (entry.isDirectory()) {
      throw new Error(`nested channel directory '${entry.name}' is not supported`);
    }
    if (!entry.isFile()) continue;

    const parsed = parse(entry.name);
    if (!VALID_EXTS.has(parsed.ext)) continue;
    if (parsed.name.startsWith(".") || parsed.name.startsWith("_")) continue;

    const url = pathToFileURL(join(dir, entry.name)).href;
    const mod = (await import(/* @vite-ignore */ url)) as Record<string, unknown>;
    const defaultExport = mod["default"];

    const definition: ChannelDefinition | undefined = isChannelExport(defaultExport)
      ? defaultExport.definition
      : isChannelDefinition(defaultExport)
        ? defaultExport
        : undefined;

    if (!definition) {
      throw new Error(
        `channel '${parsed.name}' (${entry.name}) must default-export a defineChannel() result`,
      );
    }
    if (seenNames.has(definition.channel)) {
      throw new Error(
        `duplicate channel '${definition.channel}' in ${entry.name} (already declared by ${seenNames.get(definition.channel)})`,
      );
    }
    seenNames.set(definition.channel, entry.name);

    found.push({ name: definition.channel, fileStem: parsed.name, definition });
  }

  return found;
}

async function dirExists(dir: string): Promise<boolean> {
  try {
    const s = await stat(dir);
    return s.isDirectory();
  } catch {
    return false;
  }
}
