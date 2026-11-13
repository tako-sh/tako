import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import {
  createNextjsFetchHandler,
  shutdownManagedNextjsServers,
} from "../src/nextjs/fetch-handler";
import type { ReadyableFetchHandler } from "../src/types";

describe("managed Next.js lifecycle", () => {
  let directory: string;
  let entry: string;
  let pidFile: string;

  async function pids(): Promise<number[]> {
    return (await readFile(pidFile, "utf8").catch(() => ""))
      .trim()
      .split("\n")
      .filter(Boolean)
      .map(Number);
  }

  function alive(pid: number): boolean {
    try {
      process.kill(pid, 0);
      return true;
    } catch {
      return false;
    }
  }

  function handler(mode = "ready", startupTimeoutMs = 1_000): ReadyableFetchHandler {
    return createNextjsFetchHandler(entry, {
      argv: [pidFile, mode],
      startupTimeoutMs,
    }) as ReadyableFetchHandler;
  }

  beforeEach(async () => {
    directory = await mkdtemp(path.join(tmpdir(), "tako-next-lifecycle-"));
    entry = path.join(directory, "server.mjs");
    pidFile = path.join(directory, "pids");
    await writeFile(
      entry,
      `
      import { appendFileSync, existsSync } from "node:fs";
      import { createServer } from "node:http";
      const [pidFile, mode] = process.argv.slice(2);
      const first = !existsSync(pidFile);
      appendFileSync(pidFile, process.pid + "\\n");
      if (mode === "retry" && first) {
        process.on("SIGTERM", () => setTimeout(() => process.exit(0), 250));
        setInterval(() => {}, 1000);
      } else {
        if (mode === "ignore-term") process.on("SIGTERM", () => {});
        createServer((_, response) => response.end(String(process.pid)))
          .listen(Number(process.env.PORT), process.env.HOST);
      }
    `,
    );
  });

  afterEach(async () => {
    // Clean up even when a regression loses the managed child's handle.
    for (const pid of await pids()) {
      if (alive(pid)) process.kill(pid, "SIGKILL");
    }
    await delay(50);
    await shutdownManagedNextjsServers().catch(() => {});
    await rm(directory, { recursive: true, force: true });
  });

  test("handlers sharing a child proxy to its managed hostname", async () => {
    const first = handler();
    const second = createNextjsFetchHandler(entry, {
      argv: [pidFile, "ready"],
      hostname: "127.0.0.2",
      startupTimeoutMs: 1_000,
    }) as ReadyableFetchHandler;

    await first.ready!();
    await second.ready!();
    const children = await pids();
    expect(children).toHaveLength(1);

    for (const fetch of [first, second]) {
      const response = await fetch(new Request("https://example.com/shared"), {});
      expect(response.status).toBe(200);
      expect(await response.text()).toBe(String(children[0]));
    }
    expect(await pids()).toEqual(children);
  });

  test("a timed-out child cannot invalidate its replacement or escape shutdown", async () => {
    const fetch = handler("retry", 500);
    await expect(fetch.ready!()).rejects.toThrow("Timed out waiting");
    await fetch.ready!();
    await delay(350);
    await fetch.ready!();
    const children = await pids();
    expect(children).toHaveLength(2);
    await shutdownManagedNextjsServers();
    expect(children.filter(alive)).toEqual([]);
  }, 15_000);

  test("signal termination completes shutdown and the handler can restart", async () => {
    const fetch = handler();
    await Promise.all([fetch.ready!(), fetch.ready!()]);
    const children = await pids();
    expect(children).toHaveLength(1);
    await shutdownManagedNextjsServers();
    expect(children.filter(alive)).toEqual([]);
    await fetch.ready!();
    expect(await pids()).toHaveLength(2);
    await Promise.all([shutdownManagedNextjsServers(), fetch.ready!()]);
    expect(await pids()).toHaveLength(3);
    await shutdownManagedNextjsServers();
    expect((await pids()).filter(alive)).toEqual([]);
  }, 15_000);

  test("shutdown cancels startup before a child is spawned", async () => {
    const fetch = handler();
    const starting = fetch.ready!();
    const outcome = starting.then(
      () => "ready",
      () => "cancelled",
    );
    await shutdownManagedNextjsServers();
    expect(await outcome).toBe("cancelled");
    expect(await pids()).toEqual([]);
  });

  test("SIGTERM leaves time for application cleanup after Next exits", async () => {
    const cleanup = path.join(directory, "app-cleaned");
    const driver = path.join(directory, "driver.ts");
    await writeFile(
      driver,
      `
      import { createNextjsFetchHandler } from ${JSON.stringify(path.resolve(import.meta.dir, "../src/nextjs/fetch-handler.ts"))};
      import { writeFileSync } from "node:fs";
      const handler = createNextjsFetchHandler(${JSON.stringify(entry)}, {argv: [${JSON.stringify(pidFile)}, "ready"]});
      await handler.ready();
      const alive = setInterval(() => {}, 1000);
      process.on("SIGTERM", () => {
        clearInterval(alive);
        setTimeout(() => writeFileSync(${JSON.stringify(cleanup)}, "done"), 400);
      });
      process.kill(process.pid, "SIGTERM");
    `,
    );
    const child = Bun.spawn([process.execPath, driver], { stdout: "pipe", stderr: "pipe" });
    try {
      expect(await child.exited).toBe(0);
      expect(await readFile(cleanup, "utf8")).toBe("done");
      expect((await pids()).filter(alive)).toEqual([]);
    } finally {
      child.kill();
    }
  }, 12_000);

  test("shutdown waits for SIGKILL when a child ignores SIGTERM", async () => {
    const fetch = handler("ignore-term");
    await fetch.ready!();
    await shutdownManagedNextjsServers();
    expect((await pids()).filter(alive)).toEqual([]);
  }, 15_000);
});
