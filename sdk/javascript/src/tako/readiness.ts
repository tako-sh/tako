/**
 * Readiness-fd writer for JS runtime entrypoints.
 *
 * Tako spawns an app process with a pipe on fd 4 expecting the resolved
 * HTTP port as `{port}\n`.
 */

import { closeSync, writeSync } from "node:fs";

/** Write to the inherited fd directly. */
export function writeViaInheritedFd(fd: number, port: number): void {
  try {
    writeSync(fd, `${port}\n`);
    closeSync(fd);
  } catch {
    // Not running under Tako or readiness pipe unavailable.
  }
}
