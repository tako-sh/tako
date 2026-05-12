import { afterEach, expect, test } from "bun:test";
import { mkdtemp, open, readFile, rm, writeFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import { spawn } from "node:child_process";
import path from "node:path";
import { tmpdir } from "node:os";

import { createEntrypoint, signalReadyPortOnFd } from "../src/tako/create-entrypoint";

const originalArgv = [...process.argv];
const originalCwd = process.cwd();
const originalAppRoot = process.env.TAKO_APP_ROOT;

afterEach(() => {
  process.argv = [...originalArgv];
  process.chdir(originalCwd);
  if (originalAppRoot === undefined) {
    delete process.env.TAKO_APP_ROOT;
  } else {
    process.env.TAKO_APP_ROOT = originalAppRoot;
  }
});

test("createEntrypoint returns run function and config", () => {
  const { run, port, setDraining } = createEntrypoint();
  expect(typeof run).toBe("function");
  expect(typeof port).toBe("number");
  expect(typeof setDraining).toBe("function");
});

test("createEntrypoint awaits optional ready hook before starting server", async () => {
  const rootDir = await mkdtemp(path.join(tmpdir(), "tako-entrypoint-"));
  const entryModule = path.join(rootDir, "entry.mjs");
  const lifecycle: string[] = [];

  try {
    (
      globalThis as typeof globalThis & { __takoEntrypointLifecycle?: string[] }
    ).__takoEntrypointLifecycle = lifecycle;
    await writeFile(
      entryModule,
      [
        "const lifecycle = globalThis.__takoEntrypointLifecycle;",
        "export default {",
        "  async ready() {",
        '    lifecycle?.push("ready");',
        "  },",
        "  async fetch() {",
        '    lifecycle?.push("fetch");',
        '    return new Response("ok");',
        "  },",
        "};",
        "",
      ].join("\n"),
      "utf8",
    );

    process.chdir(rootDir);
    process.argv = ["node", "entrypoint", entryModule, "--instance", "i-1"];

    const { run } = createEntrypoint();
    await run(async () => {
      lifecycle.push("start-server");
      return 4321;
    });

    expect(lifecycle).toEqual(["ready", "start-server"]);
  } finally {
    delete (globalThis as typeof globalThis & { __takoEntrypointLifecycle?: string[] })
      .__takoEntrypointLifecycle;
    await rm(rootDir, { recursive: true, force: true });
  }
});

test("internal status uses TAKO_BUILD and instance arg for runtime identity", async () => {
  const rootDir = await mkdtemp(path.join(tmpdir(), "tako-status-"));
  const entryModule = path.join(rootDir, "entry.mjs");
  const { injectBootstrap } = await import("../src/tako/secrets");

  try {
    process.env["TAKO_BUILD"] = "build-123";
    process.env["TAKO_APP_NAME"] = "entry-app/production";
    process.env["TAKO_INTERNAL_SOCKET"] = path.join(rootDir, "internal.sock");
    injectBootstrap({ token: "token-123", secrets: {} });
    await writeFile(entryModule, 'export default () => new Response("ok");\n', "utf8");

    process.chdir(rootDir);
    process.argv = ["node", "entrypoint", entryModule, "--instance", "i-1"];

    const { run } = createEntrypoint();
    await run(async (handleRequest) => {
      const response = await handleRequest(
        new Request("http://entry-app.tako/status", {
          headers: { "x-tako-internal-token": "token-123" },
        }),
      );
      const body = (await response.json()) as {
        app: string;
        version: string;
        instance_id: string;
      };
      expect(body.app).toBe("entry-app");
      expect(body.version).toBe("build-123");
      expect(body.instance_id).toBe("i-1");
      return 4321;
    });
  } finally {
    delete process.env["TAKO_BUILD"];
    delete process.env["TAKO_APP_NAME"];
    delete process.env["TAKO_INTERNAL_SOCKET"];
    injectBootstrap({ token: null, secrets: {} });
    await rm(rootDir, { recursive: true, force: true });
  }
});

test("normalizes framework response shims before returning to the runtime server", async () => {
  const rootDir = await mkdtemp(path.join(tmpdir(), "tako-response-shim-"));
  const entryModule = path.join(rootDir, "entry.mjs");

  try {
    await writeFile(
      entryModule,
      [
        "const native = new Response('ok', { status: 203 });",
        "const shim = Object.create(Response.prototype, {",
        "  _response: { get: () => native },",
        "});",
        "export default () => shim;",
        "",
      ].join("\n"),
      "utf8",
    );

    process.chdir(rootDir);
    process.argv = ["node", "entrypoint", entryModule, "--instance", "i-1"];

    const { run } = createEntrypoint();
    await run(async (handleRequest) => {
      const response = await handleRequest(new Request("http://example.com/"));
      expect(response.status).toBe(203);
      expect(await response.text()).toBe("ok");
      return 4321;
    });
  } finally {
    await rm(rootDir, { recursive: true, force: true });
  }
});

test("resolves relative main against process.cwd(), not the SDK module URL", async () => {
  // Regression test: production spawner passes `main` as a path relative to
  // the app cwd (e.g. "dist/server/tako-entry.mjs"). A naive `import(main)`
  // resolves that against the SDK's own bundled module location under
  // `node_modules/tako.sh/dist/`, not the app dir — and dies with
  // "Cannot find module". The fix converts the path to a file:// URL
  // rooted at cwd.
  const appDir = await mkdtemp(path.join(tmpdir(), "tako-rel-main-"));
  const sub = path.join(appDir, "dist", "server");
  await writeFile(path.join(appDir, "placeholder"), "", "utf8");
  const { mkdir } = await import("node:fs/promises");
  await mkdir(sub, { recursive: true });
  const entryModule = path.join(sub, "tako-entry.mjs");
  await writeFile(entryModule, 'export default () => new Response("ok");\n', "utf8");

  const originalCwd = process.cwd();
  try {
    process.chdir(appDir);
    process.argv = ["node", "entrypoint", "dist/server/tako-entry.mjs", "--instance", "i-1"];

    const { run } = createEntrypoint();
    let started = false;
    await run(async () => {
      started = true;
      return 4321;
    });
    expect(started).toBe(true);
  } finally {
    process.chdir(originalCwd);
    await rm(appDir, { recursive: true, force: true });
  }
});

test("readiness signal writes the bound port to a pipe fd instead of stdout", async () => {
  const rootDir = await mkdtemp(path.join(tmpdir(), "tako-ready-fd-"));
  const readyPipe = path.join(rootDir, "ready.pipe");

  try {
    const mkfifo = spawn("mkfifo", [readyPipe], {
      cwd: path.resolve(import.meta.dir, ".."),
      stdio: ["ignore", "ignore", "pipe"],
    });
    await new Promise<void>((resolve, reject) => {
      mkfifo.once("exit", (code) => {
        if (code === 0) {
          resolve();
        } else {
          reject(new Error(`mkfifo exited with code ${code}`));
        }
      });
    });

    const readyReader = spawn("cat", [readyPipe], {
      cwd: path.resolve(import.meta.dir, ".."),
      stdio: ["ignore", "pipe", "pipe"],
    });
    const readyLinePromise = Promise.race([
      new Promise<string>((resolve, reject) => {
        const stream = readyReader.stdout;
        if (!stream || typeof stream === "number") {
          reject(new Error("expected fd 4 pipe"));
          return;
        }

        const rl = createInterface({ input: stream });
        let sawLine = false;
        rl.once("line", (line) => {
          sawLine = true;
          rl.close();
          resolve(line);
        });
        rl.once("close", () => {
          if (!sawLine) {
            reject(new Error("fd 4 closed before readiness signal"));
          }
        });
      }),
      new Promise<string>((_, reject) =>
        setTimeout(() => reject(new Error("timed out waiting for readiness signal")), 5_000),
      ),
    ]);

    const readyWriter = await open(readyPipe, "w");
    signalReadyPortOnFd(readyWriter.fd, 4321);

    const readyLine = await readyLinePromise;
    expect(readyLine).toBe("4321");
    await new Promise<void>((resolve) => readyReader.once("exit", () => resolve()));
  } finally {
    await rm(rootDir, { recursive: true, force: true });
  }
});

test("bun-server entrypoint bootstraps from fd 3 before creating the entrypoint", async () => {
  const source = await readFile(
    path.join(import.meta.dir, "..", "src", "tako", "entrypoints", "bun-server.ts"),
    "utf8",
  );

  const bootstrapCall = source.indexOf("initBootstrapFromFd(readViaInheritedFd);");
  const createCall = source.indexOf("createEntrypoint();");

  expect(bootstrapCall).toBeGreaterThanOrEqual(0);
  expect(createCall).toBeGreaterThanOrEqual(0);
  expect(bootstrapCall).toBeLessThan(createCall);
});

test("production HTTP entrypoints install fatal error logging before boot", async () => {
  for (const entrypoint of ["bun-server.ts", "node-server.ts"]) {
    const source = await readFile(
      path.join(import.meta.dir, "..", "src", "tako", "entrypoints", entrypoint),
      "utf8",
    );

    const stdioCall = source.indexOf('installStdioBridge("app");');
    const errorCall = source.indexOf('installErrorHooks("app");');
    const consoleCall = source.indexOf('installConsoleBridge("app");');
    const createCall = source.indexOf("createEntrypoint();");

    expect(stdioCall, entrypoint).toBeGreaterThanOrEqual(0);
    expect(errorCall, entrypoint).toBeGreaterThanOrEqual(0);
    expect(consoleCall, entrypoint).toBeGreaterThanOrEqual(0);
    expect(createCall, entrypoint).toBeGreaterThanOrEqual(0);
    expect(stdioCall, entrypoint).toBeLessThan(errorCall);
    expect(errorCall, entrypoint).toBeLessThan(consoleCall);
    expect(consoleCall, entrypoint).toBeLessThan(createCall);
  }
});
