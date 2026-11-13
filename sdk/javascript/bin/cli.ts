#!/usr/bin/env node

import { parseArgs } from "node:util";

const help = `Usage: tako.sh healthcheck [url] [--timeout <ms>]

Check an HTTP endpoint once. Exit 0 for HTTP 2xx, or 1 on failure.

  url          Overrides TAKO_HEALTHCHECK_URL; defaults to
               http://127.0.0.1:\${PORT:-3000}/health
  --timeout    Overrides TAKO_HEALTHCHECK_TIMEOUT_MS (default: 2000)
  -h, --help   Show this help
`;

try {
  const { values, positionals } = parseArgs({
    options: {
      help: { type: "boolean", short: "h" },
      timeout: { type: "string" },
    },
    allowPositionals: true,
  });
  const [command, urlArgument, ...extra] = positionals;

  if (values.help && (!command || command === "healthcheck")) {
    process.stdout.write(help);
  } else {
    if (command !== "healthcheck" || extra.length > 0) {
      throw new Error("Usage: tako.sh healthcheck [url] [--timeout <ms>]");
    }

    const url = new URL(
      urlArgument ??
        process.env["TAKO_HEALTHCHECK_URL"] ??
        `http://127.0.0.1:${process.env["PORT"] || "3000"}/health`,
    );
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new Error("URL must use http:// or https://");
    }

    const timeout = Number(values.timeout ?? process.env["TAKO_HEALTHCHECK_TIMEOUT_MS"] ?? 2000);
    if (!Number.isInteger(timeout) || timeout <= 0 || timeout > 2_147_483_647) {
      throw new Error("Timeout must be an integer between 1 and 2147483647 milliseconds");
    }

    const signal = AbortSignal.timeout(timeout);
    try {
      const response = await fetch(url, { signal, redirect: "manual" });
      await response.body?.cancel();
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`);
      }
    } catch (error) {
      if (signal.aborted) {
        throw new Error(`Timed out after ${timeout}ms`);
      }
      throw error;
    }
  }
} catch (error) {
  console.error(`healthcheck: ${error instanceof Error ? error.message : String(error)}`);
  process.exitCode = 1;
}
