/**
 * Server-only readers for the Tako bootstrap envelope.
 *
 * Tako spawns each app process with a pipe on fd 3 containing a JSON
 * envelope `{"token": ..., "secrets": {...}, "storages": {...}}`, or with
 * `TAKO_BOOTSTRAP_DATA` when the app is packaged as a container.
 * Server/worker entrypoints call `initBootstrapFromFd(reader)` at startup
 * — before the user's module is imported — to populate the pure
 * `secrets.ts` state.
 *
 * Kept separate from `./secrets.ts` so that `tako.sh/internal`'s
 * `loadSecrets` re-export stays free of `node:fs` in consumer graphs.
 */

import { closeSync, fstatSync, readFileSync } from "node:fs";
import { injectBootstrap } from "./secrets";

const BOOTSTRAP_ENV = "TAKO_BOOTSTRAP_DATA";

/** Read the envelope from the container bootstrap environment variable. */
export function readViaBootstrapEnv(): string | null {
  const data = process.env[BOOTSTRAP_ENV];
  if (typeof data !== "string" || data.length === 0) return null;
  delete process.env[BOOTSTRAP_ENV];
  return data;
}

/** Read the envelope from the inherited fd 3 directly. */
export function readViaInheritedFd(): string | null {
  try {
    // Guard against blocking on a non-Tako inherited fd (e.g. GitHub Actions).
    const stat = fstatSync(3);
    if (!stat.isFIFO()) return null;
    const data = readFileSync(3, "utf-8");
    closeSync(3);
    return data;
  } catch {
    return null;
  }
}

/** Read the envelope from the active Tako bootstrap transport. */
export function readBootstrapData(): string | null {
  return readViaBootstrapEnv() ?? readViaInheritedFd();
}

/** Run a reader, parse the JSON envelope, and populate token + secrets. */
export function initBootstrapFromFd(reader: () => string | null): void {
  const data = reader();
  if (data === null) return;
  let parsed: unknown;
  try {
    parsed = JSON.parse(data);
  } catch {
    console.error("Tako: invalid bootstrap JSON on fd 3");
    process.exit(1);
  }
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    Array.isArray(parsed) ||
    typeof (parsed as { token?: unknown }).token !== "string" ||
    typeof (parsed as { secrets?: unknown }).secrets !== "object" ||
    (parsed as { secrets: unknown }).secrets === null ||
    Array.isArray((parsed as { secrets: unknown }).secrets) ||
    ("storages" in parsed &&
      (typeof (parsed as { storages?: unknown }).storages !== "object" ||
        (parsed as { storages?: unknown }).storages === null ||
        Array.isArray((parsed as { storages?: unknown }).storages)))
  ) {
    console.error(
      "Tako: bootstrap on fd 3 must be {token: string, secrets: object, storages?: object}",
    );
    process.exit(1);
  }
  const envelope = parsed as {
    token: string;
    secrets: Record<string, string>;
    storages?: Record<string, unknown>;
  };
  injectBootstrap({
    token: envelope.token,
    secrets: envelope.secrets,
    storages: envelope.storages,
  });
}
