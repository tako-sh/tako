import { join } from "node:path";
import { discoverChannels } from "./discovery";
import { ChannelRegistry } from "../channels";
import { resolveAppRootDir } from "../tako/app-root";

const CHANNELS_DIRNAME = "channels";

export interface ChannelBootstrapOptions {
  appDir: string;
  appRoot?: string;
}

export interface ChannelBootstrapResult {
  registry: ChannelRegistry;
  channelCount: number;
}

/**
 * Discover channels from `<appRoot>/channels/` and return a fresh
 * {@link ChannelRegistry} populated with them. Callers hold the registry
 * for the life of the process and pass it to the endpoints handler when
 * authorizing or dispatching.
 */
export async function bootstrapChannels(
  opts: ChannelBootstrapOptions,
): Promise<ChannelBootstrapResult> {
  const dir = join(resolveAppRootDir(opts.appDir, opts.appRoot), CHANNELS_DIRNAME);
  const found = await discoverChannels(dir);
  const registry = new ChannelRegistry();
  for (const { name, definition } of found) {
    registry.register(name, definition);
  }
  return { registry, channelCount: found.length };
}
