import { afterEach, describe, expect, test } from "bun:test";
import { resolve } from "node:path";

const cli = resolve(import.meta.dir, "../bin/cli.ts");
const servers: ReturnType<typeof Bun.serve>[] = [];

afterEach(() => {
  for (const server of servers.splice(0)) server.stop(true);
});

function serve(fetch: (request: Request) => Response | Promise<Response>) {
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch });
  servers.push(server);
  return server;
}

async function run(args: string[], overrides: Record<string, string> = {}) {
  const env = { ...process.env };
  delete env["PORT"];
  delete env["TAKO_HEALTHCHECK_URL"];
  delete env["TAKO_HEALTHCHECK_TIMEOUT_MS"];
  const child = Bun.spawn([process.execPath, cli, ...args], {
    env: { ...env, ...overrides },
    stdout: "pipe",
    stderr: "pipe",
    timeout: 3000,
  });
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ]);
  return { exitCode, stdout, stderr };
}

describe("tako.sh healthcheck", () => {
  test.each([200, 204, 299])("exits silently for HTTP %i", async (status) => {
    let requests = 0;
    const server = serve((request) => {
      requests++;
      expect(request.method).toBe("GET");
      return new Response(null, { status });
    });
    expect(await run(["healthcheck", server.url.href])).toEqual({
      exitCode: 0,
      stdout: "",
      stderr: "",
    });
    expect(requests).toBe(1);
  });

  test.each([302, 401, 503])(
    "fails once for HTTP %i without following redirects",
    async (status) => {
      let requests = 0;
      const server = serve((request) => {
        requests++;
        return new Response(null, {
          status: new URL(request.url).pathname === "/healthy" ? 200 : status,
          headers: { location: "/healthy" },
        });
      });
      expect(await run(["healthcheck", server.url.href])).toEqual({
        exitCode: 1,
        stdout: "",
        stderr: `healthcheck: HTTP ${status}\n`,
      });
      expect(requests).toBe(1);
    },
  );

  test("uses the URL argument before environment values", async () => {
    const server = serve(() => new Response("ok"));
    const result = await run(["healthcheck", server.url.href], {
      TAKO_HEALTHCHECK_URL: "invalid",
      PORT: "invalid",
    });
    expect(result.exitCode).toBe(0);
  });

  test("uses TAKO_HEALTHCHECK_URL before PORT", async () => {
    const server = serve(() => new Response("ok"));
    const result = await run(["healthcheck"], {
      TAKO_HEALTHCHECK_URL: server.url.href,
      PORT: "invalid",
    });
    expect(result.exitCode).toBe(0);
  });

  test("defaults to loopback /health using PORT", async () => {
    const server = serve((request) => {
      expect(new URL(request.url).pathname).toBe("/health");
      return new Response("ok");
    });
    expect((await run(["healthcheck"], { PORT: String(server.port) })).exitCode).toBe(0);
  });

  test.each(["argument", "environment"])(
    "bounds a stalled request using the %s timeout",
    async (source) => {
      const server = serve(() => new Promise<Response>(() => {}));
      const args = ["healthcheck", server.url.href];
      if (source === "argument") args.push("--timeout", "50");
      expect(
        await run(args, {
          TAKO_HEALTHCHECK_TIMEOUT_MS: source === "argument" ? "invalid" : "50",
        }),
      ).toEqual({
        exitCode: 1,
        stdout: "",
        stderr: "healthcheck: Timed out after 50ms\n",
      });
    },
  );

  test("does not wait for a streaming response body", async () => {
    const server = serve(
      () =>
        new Response(
          new ReadableStream({
            start(controller) {
              controller.enqueue(new TextEncoder().encode("ok"));
            },
          }),
        ),
    );
    expect((await run(["healthcheck", server.url.href, "--timeout", "500"])).exitCode).toBe(0);
  });

  test("fails on a refused connection", async () => {
    const server = serve(() => new Response("ok"));
    const url = server.url.href;
    server.stop(true);
    const result = await run(["healthcheck", url]);
    expect(result.exitCode).toBe(1);
    expect(result.stdout).toBe("");
    expect(result.stderr).toStartWith("healthcheck: ");
    expect(result.stderr).not.toContain("\n    at ");
  });

  test.each([
    [],
    ["unknown"],
    ["healthcheck", "invalid"],
    ["healthcheck", "file:///tmp/health"],
    ["healthcheck", "http://127.0.0.1", "extra"],
    ["healthcheck", "--unknown"],
    ["healthcheck", "--timeout"],
    ["healthcheck", "--timeout=0"],
    ["healthcheck", "--timeout=-1"],
    ["healthcheck", "--timeout=1.5"],
    ["healthcheck", "--timeout=invalid"],
    ["healthcheck", "--timeout=2147483648"],
  ])("rejects invalid arguments %j", async (...args) => {
    const result = await run(args);
    expect(result.exitCode).toBe(1);
    expect(result.stdout).toBe("");
    expect(result.stderr).toStartWith("healthcheck: ");
  });

  test.each([["--help"], ["healthcheck", "--help"]])("shows help for %j", async (...args) => {
    const result = await run(args);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("Usage: tako.sh healthcheck");
    expect(result.stderr).toBe("");
  });
});
