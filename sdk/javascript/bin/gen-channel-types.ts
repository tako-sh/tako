#!/usr/bin/env bun

import { resolve } from "node:path";
import { discoverChannels } from "../src/channels/discovery";

export async function generateChannelTypes(channelsDir: string): Promise<string> {
  const discovered = await discoverChannels(channelsDir);
  const lines = [
    "export interface TakoChannels {",
    ...discovered.map((channel) => {
      const params = schemaToType(channel.definition.paramsSchema);
      const transport = channel.definition.transport === "ws" ? '"ws"' : '"sse"';
      return `  ${JSON.stringify(channel.name)}: { params: ${params}; messages: Record<string, unknown>; transport: ${transport}; };`;
    }),
    "}",
    "",
  ];
  return lines.join("\n");
}

function schemaToType(schema: unknown): string {
  if (!isRecord(schema)) return "Record<string, unknown>";
  const type = schema["type"];
  switch (type) {
    case "object":
      return objectSchemaToType(schema);
    case "array":
      return `${schemaToType(schema["items"])}[]`;
    case "integer":
    case "number":
      return "number";
    case "boolean":
      return "boolean";
    case "string":
      return "string";
    default:
      return "unknown";
  }
}

function objectSchemaToType(schema: Record<string, unknown>): string {
  const properties = isRecord(schema["properties"]) ? schema["properties"] : {};
  const required = new Set(Array.isArray(schema["required"]) ? schema["required"] : []);
  const entries = Object.entries(properties);
  if (entries.length === 0) return "Record<string, never>";
  return `{ ${entries
    .map(
      ([key, value]) =>
        `${propertyName(key)}${required.has(key) ? "" : "?"}: ${schemaToType(value)};`,
    )
    .join(" ")} }`;
}

function propertyName(key: string): string {
  return /^[A-Za-z_$][\w$]*$/.test(key) ? key : JSON.stringify(key);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

if (import.meta.main) {
  const channelsDir = Bun.argv[2];
  if (!channelsDir) {
    console.error("usage: tako-sh-gen-channel-types <channels-dir>");
    process.exit(2);
  }
  process.stdout.write(await generateChannelTypes(resolve(channelsDir)));
}
