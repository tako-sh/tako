#!/usr/bin/env bun

import { resolve } from "node:path";
import { discoverServices } from "../src/services/discovery";
import { generateServiceDocument, type ServiceOutputFormat } from "../src/services/generate";

export async function generateServiceTypes(
  servicesDir: string,
  format: ServiceOutputFormat = "typescript",
): Promise<string> {
  return generateServiceDocument(await discoverServices(servicesDir), format);
}

if (import.meta.main) {
  const [servicesDir, flag, format, ...extra] = Bun.argv.slice(2);
  if (
    !servicesDir ||
    (flag !== undefined &&
      (flag !== "--format" || !["typescript", "manifest", "openapi"].includes(format ?? ""))) ||
    extra.length
  ) {
    console.error(
      "usage: tako-sh-gen-service-types <services-dir> [--format typescript|manifest|openapi]",
    );
    process.exit(2);
  }
  try {
    process.stdout.write(
      await generateServiceTypes(resolve(servicesDir), format as ServiceOutputFormat | undefined),
    );
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }
}
