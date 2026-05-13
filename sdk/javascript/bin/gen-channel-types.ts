#!/usr/bin/env bun

import { resolve } from "node:path";
import { discoverChannels } from "../src/channels/discovery";

export async function generateChannelTypes(channelsDir: string): Promise<string> {
  const discovered = await discoverChannels(channelsDir);
  const lines = [
    "/**",
    " * Project-specific channel metadata discovered from `<app_root>/channels/`.",
    " *",
    " * Used by `tako.sh/client` and `tako.sh/react` to type route params, messages, and transports.",
    " */",
    "export interface TakoChannels {",
    ...discovered.flatMap((channel) => {
      return [
        `  /** Channel \`${escapeJsdocText(JSON.stringify(channel.name))}\` route params, message metadata, and transport. */`,
        `  ${JSON.stringify(channel.name)}: import("tako.sh").InferChannel<typeof import(${JSON.stringify(`./channels/${channel.fileStem}`)}).default>;`,
      ];
    }),
    "}",
    "",
  ];
  return lines.join("\n");
}

function escapeJsdocText(value: string): string {
  return value.replaceAll("*/", "* /");
}

if (import.meta.main) {
  const channelsDir = Bun.argv[2];
  if (!channelsDir) {
    console.error("usage: tako-sh-gen-channel-types <channels-dir>");
    process.exit(2);
  }
  process.stdout.write(await generateChannelTypes(resolve(channelsDir)));
}
