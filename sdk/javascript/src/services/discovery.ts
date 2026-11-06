import { readdir } from "node:fs/promises";
import { join, parse } from "node:path";
import { pathToFileURL } from "node:url";
import { defineService, type ServiceDefinition } from "./contract";

/** Imports explicit declarations from a flat app-root services directory. */
export async function discoverServices(dir: string): Promise<ServiceDefinition[]> {
  const entries = await readdir(dir, { withFileTypes: true }).catch(
    (error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return [];
      throw error;
    },
  );
  const services: ServiceDefinition[] = [];
  const names = new Set<string>();
  for (const entry of entries.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0))) {
    if (/^[._]/.test(entry.name) || entry.name.endsWith(".d.ts") || entry.name.endsWith(".d.mts"))
      continue;
    if (entry.isDirectory())
      throw new Error(`Nested service directory '${entry.name}' is not supported.`);
    if (!entry.isFile() || ![".ts", ".mts", ".js", ".mjs"].includes(parse(entry.name).ext))
      continue;
    const mod = (await import(
      /* @vite-ignore */ pathToFileURL(join(dir, entry.name)).href
    )) as Record<string, unknown>;
    const service = mod["default"] as ServiceDefinition | undefined;
    if (
      !service ||
      typeof service.name !== "string" ||
      !service.operations ||
      typeof service.operations !== "object"
    )
      throw new Error(`Service '${entry.name}' must default-export a defineService() result.`);
    if (names.has(service.name)) throw new Error(`Duplicate service '${service.name}'.`);
    names.add(service.name);
    services.push(defineService(service.name, service.operations));
  }
  return services.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}
