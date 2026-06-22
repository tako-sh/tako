import { join } from "node:path";
import { discoverChannels } from "./discovery";
import { ChannelRegistry } from "../channels";
import { resolveAppRootDir } from "../tako/app-root";

const CHANNELS_DIRNAME = "channels";

/** Options for bootstrapping channel discovery. */
export interface ChannelBootstrapOptions {
  /** App directory that contains the configured app root. */
  appDir: string;
  /**
   * JavaScript app root relative to `appDir`.
   * @defaultValue "src"
   */
  appRoot?: string;
}

/** Result returned by {@link bootstrapChannels}. */
export interface ChannelBootstrapResult {
  /** Registry populated with discovered channel definitions. */
  registry: ChannelRegistry;
  /** Number of channel modules discovered. */
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
