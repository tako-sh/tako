/**
 * E2E tests for `tako dev` - runs against real fixtures.
 * Skipped unless TAKO_DEV_E2E=1 is set.
 */
import { describe, test, expect } from "bun:test";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, cpSync, symlinkSync } from "fs";
import { join, resolve } from "path";
import { tmpdir } from "os";

const SKIP = !process.env["TAKO_DEV_E2E"];
const TAKO_BIN =
  process.env["TAKO_BIN"] ??
  resolve(import.meta.dirname, "..", "..", "..", "target", "debug", "tako");
const FIXTURES_DIR = resolve(import.meta.dirname, "..", "..", "fixtures", "javascript");
const SDK_DIR = resolve(import.meta.dirname, "..", "..", "..", "sdk", "javascript");

function safeRead(path: string): string {
  try {
    return readFileSync(path, "utf-8");
  } catch {
    return "";
  }
}

/** Copy a fixture to a temp dir and symlink the SDK. */
function prepareFixture(name: string) {
  const tempDir = mkdtempSync(join(tmpdir(), `tako-dev-e2e-${name}-`));
  const pd = join(tempDir, "app");
  cpSync(join(FIXTURES_DIR, name), pd, { recursive: true });

  // Symlink the SDK so the entrypoint is available without npm install.
  mkdirSync(join(pd, "node_modules"), { recursive: true });
  const sdkLink = join(pd, "node_modules", "tako.sh");
  rmSync(sdkLink, { recursive: true, force: true });
  symlinkSync(SDK_DIR, sdkLink);

  const lf = join(tempDir, "dev.log");
  return { tempDir, pd, lf };
}

function startDev(pd: string, lf: string) {
  return Bun.spawn(["sh", "-c", `exec "${TAKO_BIN}" dev > "${lf}" 2>&1`], {
    cwd: pd,
    env: { ...process.env, TERM: "dumb", NO_COLOR: "1" },
    stdin: "ignore",
    stdout: "ignore",
    stderr: "ignore",
  });
}

function appUrl(baseUrl: string, path: string): string {
  return new URL(path, baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`).toString();
}

async function postJson(baseUrl: string, path: string, body: unknown): Promise<Response> {
  return await fetch(appUrl(baseUrl, path), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
    tls: { rejectUnauthorized: false },
  });
}

async function waitForHttpOk(baseUrl: string, lf: string, timeoutMs = 10_000): Promise<void> {
  let last: { status: number; body: string } | null = null;
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    try {
      const response = await fetch(baseUrl, {
        // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
        tls: { rejectUnauthorized: false },
      });
      if (response.status === 200) {
        return;
      }
      last = { status: response.status, body: await response.text() };
    } catch (error) {
      last = { status: 0, body: String(error) };
    }

    await Bun.sleep(250);
  }

  throw new Error(
    `App route never became ready. Last response: ${last?.status ?? "none"} ${
      last?.body ?? ""
    }\nLog:\n${safeRead(lf)}`,
  );
}

async function collectSseUntil(
  url: string,
  expected: readonly string[],
  ready: () => void,
  signal: AbortSignal,
): Promise<string> {
  const resp = await fetch(url, {
    headers: { accept: "text/event-stream", authorization: "Bearer e2e" },
    signal,
    // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
    tls: { rejectUnauthorized: false },
  });
  expect(resp.status).toBe(200);
  expect(resp.headers.get("content-type") ?? "").toContain("text/event-stream");
  ready();

  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let received = "";
  while (true) {
    const { done, value } = await reader.read();
    if (done) return received;
    received += decoder.decode(value, { stream: true });
    if (expected.every((message) => received.includes(message))) {
      return received;
    }
  }
}

/**
 * Wait for the dev server to be ready.
 * Returns the dev URL printed by `tako dev` (e.g. https://bun-e2e.test/).
 * Readiness is signalled by "App started" in the log.
 */
async function waitForApp(lf: string, timeoutMs = 60_000): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const log = safeRead(lf);
    if (/App started/.test(log)) {
      const m = log.match(/^(https?:\/\/\S+)/m);
      if (m?.[1]) return m[1];
    }
    await Bun.sleep(300);
  }
  throw new Error(`App didn't start.\nLog:\n${safeRead(lf)}`);
}

/**
 * Wait for the app process PID to appear in the log ("App pid <n>").
 * The runner emits this line in non-interactive mode.
 */
async function waitForAppPid(lf: string, timeoutMs = 30_000): Promise<number> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const m = safeRead(lf).match(/^App pid (\d+)/m);
    if (m) return Number(m[1]);
    await Bun.sleep(300);
  }
  throw new Error(`App pid never appeared.\nLog:\n${safeRead(lf)}`);
}

describe.skipIf(SKIP)("tako dev fixtures", () => {
  for (const runtime of ["bun", "node"]) {
    test(`${runtime}: starts and serves HTTP`, async () => {
      const { tempDir, pd, lf } = prepareFixture(runtime);
      const proc = startDev(pd, lf);

      try {
        const devUrl = await waitForApp(lf);

        // Fixtures serve HTML at /.
        const resp = await fetch(devUrl, {
          // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
          tls: { rejectUnauthorized: false },
        });
        expect(resp.status).toBe(200);
        const body = await resp.text();
        expect(body).toContain("Tako app");
      } finally {
        try {
          process.kill(proc.pid, "SIGKILL");
        } catch {}
        rmSync(tempDir, { recursive: true, force: true });
      }
    }, 90_000);
  }

  test("bun: detects process exit", async () => {
    const { tempDir, pd, lf } = prepareFixture("bun");
    const proc = startDev(pd, lf);

    try {
      await waitForApp(lf);

      // Wait for the app PID to appear in the log, then kill it directly.
      const appPid = await waitForAppPid(lf);
      try {
        process.kill(appPid, "SIGKILL");
      } catch {}

      // Wait for exit detection.
      for (let i = 0; i < 20; i++) {
        await Bun.sleep(500);
        if (/App exited \(killed by signal/.test(safeRead(lf))) break;
      }
      expect(safeRead(lf)).toMatch(/App exited \(killed by signal/);
    } finally {
      try {
        process.kill(proc.pid, "SIGKILL");
      } catch {}
      rmSync(tempDir, { recursive: true, force: true });
    }
  }, 90_000);

  test("channels-workflows: streams direct and workflow publishes over SSE", async () => {
    const { tempDir, pd, lf } = prepareFixture("channels-workflows");
    const proc = startDev(pd, lf);
    const abort = new AbortController();

    try {
      const devUrl = await waitForApp(lf);
      await waitForHttpOk(devUrl, lf);

      const directMessage = `direct-${Date.now()}`;
      const workflowMessage = `workflow-${Date.now()}`;
      let markReady!: () => void;
      const ready = new Promise<void>((resolve) => {
        markReady = resolve;
      });
      const sse = collectSseUntil(
        appUrl(devUrl, "/channels/demo"),
        [directMessage, workflowMessage],
        markReady,
        abort.signal,
      );

      await ready;

      const direct = await postJson(devUrl, "/publish", { message: directMessage });
      expect(direct.status).toBe(200);
      expect(await direct.json()).toMatchObject({ ok: true });

      const workflow = await postJson(devUrl, "/enqueue", { message: workflowMessage });
      expect(workflow.status).toBe(200);
      expect(await workflow.json()).toMatchObject({ ok: true });

      const received = await Promise.race([
        sse,
        Bun.sleep(20_000).then(() => {
          throw new Error(`Timed out waiting for SSE messages.\nLog:\n${safeRead(lf)}`);
        }),
      ]);
      expect(received).toContain(directMessage);
      expect(received).toContain(workflowMessage);
    } finally {
      abort.abort();
      try {
        process.kill(proc.pid, "SIGKILL");
      } catch {}
      rmSync(tempDir, { recursive: true, force: true });
    }
  }, 120_000);
});
