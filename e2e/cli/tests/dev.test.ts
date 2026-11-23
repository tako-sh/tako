/**
 * E2E tests for `tako dev` - runs against real fixtures.
 * Skipped unless TAKO_DEV_E2E=1 is set.
 */
import { describe, test, expect } from "bun:test";
import {
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  cpSync,
  symlinkSync,
  writeFileSync,
  renameSync,
} from "fs";
import { join, resolve } from "path";
import { tmpdir } from "os";

const SKIP = !process.env["TAKO_DEV_E2E"];
const TAKO_BIN =
  process.env["TAKO_BIN"] ??
  resolve(import.meta.dirname, "..", "..", "..", "target", "debug", "tako");
const FIXTURES_DIR = resolve(import.meta.dirname, "..", "..", "fixtures", "javascript");
const SDK_DIR = resolve(import.meta.dirname, "..", "..", "..", "sdk", "javascript");
const CLI_MODULES = resolve(import.meta.dirname, "..", "node_modules");

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
  // Fixtures reach their database bindings through @libsql/client.
  mkdirSync(join(pd, "node_modules", "@libsql"), { recursive: true });
  symlinkSync(
    join(CLI_MODULES, "@libsql", "client"),
    join(pd, "node_modules", "@libsql", "client"),
  );
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

async function postJsonWhenReady(
  baseUrl: string,
  path: string,
  body: unknown,
  lf: string,
  timeoutMs = 10_000,
): Promise<Response> {
  let last: { status: number; body: string } | null = null;
  const deadline = Date.now() + timeoutMs;

  while (Date.now() < deadline) {
    const response = await postJson(baseUrl, path, body);
    if (![502, 503].includes(response.status)) {
      return response;
    }

    last = { status: response.status, body: await response.text() };
    await Bun.sleep(250);
  }

  throw new Error(
    `POST ${path} never became ready. Last response: ${last?.status ?? "none"} ${
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
  test("custom environments coexist and restart/stop independently", async () => {
    const { tempDir, pd } = prepareFixture("bun");
    const appName = `dev-env-${"a".repeat(47)}`;
    // Exercise explicit config selection without a default tako.toml.
    renameSync(join(pd, "tako.toml"), join(pd, "app.toml"));
    writeFileSync(
      join(pd, "app.toml"),
      `
name = "${appName}"
runtime = "bun"
[vars.development]
GREETING = "development"
[vars.test-e2e]
GREETING = "test"
[envs.development]
db = { main = "same-name" }
[envs.test-e2e]
# An external-only route keeps the generated local default enabled.
route = "test-e2e.example.com"
db = { main = "same-name" }
`,
    );
    mkdirSync(join(pd, "migrations"), { recursive: true });
    writeFileSync(join(pd, "migrations/001_items.sql"), "CREATE TABLE items (value TEXT);");
    writeFileSync(
      join(pd, "index.ts"),
      `
import { tako } from "tako.sh";
import { createClient } from "@libsql/client";
export default async function fetch(request: Request) {
  const db = createClient(tako.db.main);
  try {
    if (request.method === "POST") await db.execute("INSERT INTO items VALUES ('item')");
    const result = await db.execute("SELECT count(*) AS count FROM items");
    return Response.json({ env: tako.env, greeting: process.env.GREETING,
      secret: tako.secrets.TEST_VALUE, envSecret: tako.secrets.ENV,
      dataDir: process.env.TAKO_DATA_DIR,
      count: Number(result.rows[0].count), pid: process.pid });
  } finally { db.close(); }
}
`,
    );
    async function cli(args: string[], stdin?: string) {
      const proc = Bun.spawn([TAKO_BIN, "-c", "app.toml", ...args, "--json"], {
        cwd: pd,
        env: { ...process.env, NO_COLOR: "1" },
        stdin: stdin === undefined ? "ignore" : new Blob([stdin]),
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, stderr, code] = await Promise.all([
        new Response(proc.stdout).text(),
        new Response(proc.stderr).text(),
        proc.exited,
      ]);
      expect(code, `${stdout}\n${stderr}`).toBe(0);
      return JSON.parse(stdout);
    }
    async function snapshot(url: string, method = "GET") {
      const response = await fetch(url, {
        method,
        // @ts-ignore - Bun extension: local dev CA.
        tls: { rejectUnauthorized: false },
      });
      expect(response.status).toBe(200);
      return response.json();
    }
    try {
      await cli(["secret", "set", "TEST_VALUE", "--env", "development"], "dev-secret");
      await cli(["secret", "set", "TEST_VALUE", "--env", "test-e2e"], "test-secret");
      await cli(["secret", "set", "ENV", "--env", "test-e2e"], "production");
      await cli(["secret", "set", "TAKO_DATA_DIR", "--env", "test-e2e"], "not-the-runtime-dir");
      const dev = await cli(["dev", "--background"]);
      const test = await cli(["dev", "--env", "test-e2e", "--background"]);
      expect(test.url).not.toBe(dev.url);
      expect(new URL(test.url).hostname.endsWith(".test")).toBe(true);
      const normal = await snapshot(dev.url, "POST");
      const isolated = await snapshot(test.url);
      expect(normal).toMatchObject({
        env: "development",
        greeting: "development",
        secret: "dev-secret",
        count: 1,
      });
      expect(isolated).toMatchObject({
        env: "test-e2e",
        greeting: "test",
        secret: "test-secret",
        envSecret: "production",
        count: 0,
      });
      expect(isolated.dataDir).not.toBe(normal.dataDir);
      expect(isolated.dataDir).toEndWith("/.tako/data/envs/test-e2e/app");
      const listed = await cli(["dev", "list", "--env", "test-e2e"]);
      expect(listed.apps.filter((app: { name: string }) => app.name === test.app)).toHaveLength(1);
      expect(listed.apps.every((app: { env: string }) => app.env === "test-e2e")).toBe(true);
      await snapshot(test.url, "POST");
      await cli(["dev", "--env", "test-e2e", "--restart", "--background"]);
      expect(await snapshot(dev.url)).toMatchObject({ pid: normal.pid, count: 1 });
      expect(await snapshot(test.url)).toMatchObject({ count: 1, env: "test-e2e" });
      expect((await snapshot(test.url)).pid).not.toBe(isolated.pid);
      await cli(["dev", "stop", "--env", "test-e2e"]);
      expect(await snapshot(dev.url)).toMatchObject({ pid: normal.pid, count: 1 });
      expect(
        (await cli(["dev", "list", "--env", "test-e2e"])).apps.some(
          (app: { name: string }) => app.name === test.app,
        ),
      ).toBe(false);
    } finally {
      for (const env of ["test-e2e", "development"]) {
        await cli(["dev", "stop", "--env", env]).catch(() => {});
      }
      rmSync(tempDir, { recursive: true, force: true });
    }
  }, 120_000);

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

  test("channels-workflows: direct and workflow publishes over SSE", async () => {
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
        appUrl(devUrl, "/_tako/channels/sse/demo"),
        [directMessage, workflowMessage],
        markReady,
        abort.signal,
      );

      await ready;

      const direct = await postJsonWhenReady(devUrl, "/publish", { message: directMessage }, lf);
      expect(direct.status).toBe(200);
      expect(await direct.json()).toMatchObject({ ok: true });

      const workflow = await postJsonWhenReady(
        devUrl,
        "/enqueue",
        { message: workflowMessage, id: workflowMessage },
        lf,
      );
      expect(workflow.status).toBe(200);
      expect(await workflow.json()).toMatchObject({ ok: true, runId: workflowMessage });
      const duplicate = await postJsonWhenReady(
        devUrl,
        "/enqueue",
        { message: workflowMessage, id: workflowMessage },
        lf,
      );
      expect(duplicate.status).toBe(200);
      expect(await duplicate.json()).toMatchObject({ ok: true, runId: workflowMessage });

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

  test("channels-workflows: a signal sent before the run waits is kept for the wait", async () => {
    const { tempDir, pd, lf } = prepareFixture("channels-workflows");
    const proc = startDev(pd, lf);
    try {
      const devUrl = await waitForApp(lf);
      await waitForHttpOk(devUrl, lf);
      const runId = `approval-${Date.now()}`;

      const enqueued = await postJsonWhenReady(
        devUrl,
        "/approval",
        { id: runId, settleMs: 5_000 },
        lf,
      );
      expect(await enqueued.json()).toMatchObject({ ok: true, runId });
      // The run is still sleeping: the signal must be buffered, not dropped.
      const early = await postJson(devUrl, "/signal", {
        runId,
        event: "decision",
        payload: { approved: true },
      });
      expect(await early.json()).toMatchObject({ ok: true, status: "buffered" });
      const duplicate = await postJson(devUrl, "/signal", {
        runId,
        event: "decision",
        payload: { approved: false },
      });
      expect(await duplicate.json()).toMatchObject({ status: "duplicate" });

      let outcome: { status: string; result?: unknown } = { status: "timeout" };
      const deadline = Date.now() + 60_000;
      while (outcome.status === "timeout" && Date.now() < deadline) {
        outcome = await (await postJson(devUrl, "/approval/result", { id: runId })).json();
      }
      expect(outcome, safeRead(lf)).toMatchObject({
        status: "succeeded",
        result: { approved: true },
      });

      const late = await postJson(devUrl, "/signal", { runId, event: "decision", payload: {} });
      expect(await late.json()).toMatchObject({ status: "finished" });
    } finally {
      try {
        process.kill(proc.pid, "SIGKILL");
      } catch {}
      rmSync(tempDir, { recursive: true, force: true });
    }
  }, 120_000);

  test("channels-workflows: Store writes persist in the dev database binding", async () => {
    const { tempDir, pd, lf } = prepareFixture("channels-workflows");
    const proc = startDev(pd, lf);
    try {
      const devUrl = await waitForApp(lf);
      await waitForHttpOk(devUrl, lf);
      const message = `stored-${Date.now()}`;
      const headers = { authorization: "Bearer e2e", "content-type": "application/json" };
      const write = await fetch(appUrl(devUrl, "/store/messages"), {
        method: "POST",
        headers,
        body: JSON.stringify({ chatId: "chat-a", message }),
        // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
        tls: { rejectUnauthorized: false },
      });
      expect(write.status).toBe(200);
      const params = encodeURIComponent(JSON.stringify({ chat: { id: "chat-a" } }));
      const read = await fetch(appUrl(devUrl, `/_tako/stores/messages?params=${params}`), {
        headers,
        // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
        tls: { rejectUnauthorized: false },
      });
      expect(read.status).toBe(200);
      const body = (await read.json()) as { data: { messages: { message: string }[] } };
      expect(body.data.messages.map((row) => row.message)).toContain(message);
      expect(existsSync(join(pd, ".tako", "data", "databases", "messages.sqlite"))).toBe(true);
    } finally {
      try {
        process.kill(proc.pid, "SIGKILL");
      } catch {}
      rmSync(tempDir, { recursive: true, force: true });
    }
  }, 90_000);

  test("presence: joins and removes disconnected tabs", async () => {
    const { tempDir, pd, lf } = prepareFixture("channels-workflows");
    const proc = startDev(pd, lf);
    const sockets: WebSocket[] = [];
    try {
      const devUrl = await waitForApp(lf);
      await waitForHttpOk(devUrl, lf);
      const url = new URL(appUrl(devUrl, "/_tako/channels/ws/tako.presence%3Alobby"));
      url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
      let firstMembers: unknown[] = [];
      for (let index = 0; index < 2; index++) {
        const socket = new WebSocket(url, {
          // `tako dev` runs the app's verifier, and the lobby admits only this header.
          // @ts-ignore - Bun extension: request headers on the upgrade.
          headers: { authorization: "Bearer e2e" },
          // @ts-ignore - Bun extension: skip TLS verification for the self-signed dev CA.
          tls: { rejectUnauthorized: false },
        });
        sockets.push(socket);
        await new Promise<void>((resolve, reject) => {
          const timer = setTimeout(
            () => reject(new Error(`Presence join timed out.\n${safeRead(lf)}`)),
            10_000,
          );
          socket.addEventListener("message", (event) => {
            const message = JSON.parse(String(event.data));
            if (message.type === "tako.presence.ready") {
              socket.send(JSON.stringify({ type: "tako.presence", data: { typing: false } }));
            } else if (message.type === "tako.presence") {
              if (index === 0) firstMembers = message.members;
              if (message.members.length === index + 1) {
                clearTimeout(timer);
                resolve();
              }
            }
          });
          socket.addEventListener("error", (event) => {
            clearTimeout(timer);
            const detail = event instanceof ErrorEvent ? event.message : event.type;
            reject(new Error(`Presence connection failed: ${detail}\nLog:\n${safeRead(lf)}`));
          });
        });
      }
      for (let attempt = 0; firstMembers.length !== 2 && attempt < 40; attempt++)
        await Bun.sleep(100);
      expect(firstMembers).toHaveLength(2);
      sockets[1]!.close();
      for (let attempt = 0; firstMembers.length !== 1 && attempt < 40; attempt++)
        await Bun.sleep(100);
      expect(firstMembers).toHaveLength(1);
    } finally {
      for (const socket of sockets) socket.close();
      try {
        process.kill(proc.pid, "SIGKILL");
      } catch {}
      rmSync(tempDir, { recursive: true, force: true });
    }
  }, 90_000);
});
