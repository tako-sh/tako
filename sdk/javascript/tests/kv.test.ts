import { afterAll, afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { join } from "node:path";
import { inspect } from "node:util";
import { createKvBag, loadKv } from "../src/kv";
import { injectBootstrap, parseBootstrapEnvelope } from "../src/tako/secrets";
import {
  APP_NAME_ENV,
  closeInternalSocketPoolsForTests,
  INTERNAL_SOCKET_ENV,
} from "../src/tako/socket";

const SESSIONS = { token: "tok-sessions" };

/** A KV store behind a fake internal socket, keyed by token and key. */
class FakeKv {
  readonly values = new Map<string, { json: string; ttl_ms?: number }>();
  private server: Server | null = null;
  private dir = "";
  path = "";

  async start(): Promise<void> {
    this.dir = await mkdtemp(join("/tmp", "tako-kv-"));
    this.path = join(this.dir, "internal.sock");
    this.server = createServer((socket) => {
      let buffered = "";
      socket.on("data", (chunk: Buffer) => {
        buffered += chunk.toString("utf8");
        for (let nl = buffered.indexOf("\n"); nl !== -1; nl = buffered.indexOf("\n")) {
          const cmd = JSON.parse(buffered.slice(0, nl)) as Record<string, string>;
          buffered = buffered.slice(nl + 1);
          socket.write(`${JSON.stringify(this.serve(cmd))}\n`);
        }
      });
    });
    await new Promise<void>((resolve, reject) => {
      this.server?.once("error", reject);
      this.server?.listen(this.path, resolve);
    });
  }

  private serve(cmd: Record<string, unknown>): unknown {
    const id = `${String(cmd["token"])}:${String(cmd["key"])}`;
    switch (cmd["operation"]) {
      case "get":
        return { status: "ok", data: this.values.get(id)?.json ?? null };
      case "put":
        this.values.set(id, {
          json: cmd["json"] as string,
          ...(cmd["ttl_ms"] !== undefined && { ttl_ms: cmd["ttl_ms"] as number }),
        });
        return { status: "ok", data: null };
      case "delete":
        this.values.delete(id);
        return { status: "ok", data: null };
      default:
        return { status: "error", code: "invalid_request", message: "unknown operation" };
    }
  }

  async stop(): Promise<void> {
    this.server?.close();
    await rm(this.dir, { recursive: true, force: true });
  }
}

const server = new FakeKv();

beforeAll(async () => {
  await server.start();
});

afterAll(async () => {
  closeInternalSocketPoolsForTests();
  await server.stop();
});

describe("tako.kv", () => {
  afterEach(() => {
    injectBootstrap({ token: null, secrets: {} });
    delete process.env[INTERNAL_SOCKET_ENV];
    delete process.env[APP_NAME_ENV];
    server.values.clear();
  });

  function connect() {
    process.env[INTERNAL_SOCKET_ENV] = server.path;
    process.env[APP_NAME_ENV] = "shop/production";
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    return loadKv<{ sessions: unknown }>();
  }

  test("puts, gets and deletes JSON values with the binding's token", async () => {
    const kv = connect();
    expect(await kv.sessions.get("session:1")).toBeUndefined();
    await kv.sessions.put("session:1", { user: "u1", roles: ["admin"] });
    expect(await kv.sessions.get<{ user: string }>("session:1")).toEqual({
      user: "u1",
      roles: ["admin"],
    });
    // JSON null is a value, unlike a missing key.
    await kv.sessions.put("nothing", null);
    expect(await kv.sessions.get("nothing")).toBeNull();
    await kv.sessions.put("challenge", "abc", { ttl: 1_500.2 });
    expect(server.values.get("tok-sessions:challenge")).toEqual({ json: '"abc"', ttl_ms: 1_501 });
    expect(server.values.get("tok-sessions:session:1")?.ttl_ms).toBeUndefined();
    await kv.sessions.delete("session:1");
    expect(await kv.sessions.get("session:1")).toBeUndefined();
  });

  test("rejects invalid keys, values and TTLs before sending", async () => {
    const kv = connect();
    await expect(kv.sessions.get("")).rejects.toThrow("KV key cannot be empty.");
    await expect(kv.sessions.get("k".repeat(1025))).rejects.toThrow("KV key exceeds 1 KiB.");
    await expect(kv.sessions.put("k", undefined)).rejects.toThrow("JSON-serializable");
    await expect(kv.sessions.put("k", "x".repeat(256 * 1024))).rejects.toThrow("exceeds 256 KiB");
    for (const ttl of [0, -1, Number.NaN, Number.POSITIVE_INFINITY]) {
      await expect(kv.sessions.put("k", 1, { ttl })).rejects.toThrow("KV ttl must be");
    }
    expect(server.values.size).toBe(0);
  });

  test("needs the internal socket", async () => {
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    await expect(loadKv()["sessions"]!.get("k")).rejects.toThrow("internal socket");
  });

  test("never shows the token when logged or serialized", () => {
    const kv = connect();
    expect(JSON.parse(JSON.stringify(kv.sessions))).toEqual({ kv: "sessions" });
    expect(inspect(kv.sessions)).toBe("[TakoKvStore sessions]");
    expect(JSON.stringify(kv)).not.toContain("tok-sessions");
    expect(inspect(kv)).not.toContain("tok-sessions");
    expect(Object.isFrozen(kv.sessions)).toBe(true);
    expect(kv.sessions).toBe(kv.sessions);
  });

  test("reads bindings on access, after the bootstrap arrives", () => {
    const kv = loadKv();
    expect(kv["sessions"]).toBeUndefined();
    expect("sessions" in kv).toBe(false);
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    expect(typeof kv["sessions"]?.get).toBe("function");
    expect("sessions" in kv).toBe(true);
    expect(Reflect.ownKeys(kv)).toEqual(["sessions"]);
    expect(Object.keys({ ...kv })).toEqual(["sessions"]);
  });

  test("rejects malformed bindings with the binding name", () => {
    const kv = createKvBag(() => ({ sessions: {}, other: "tok" }));
    expect(() => kv["sessions"]).toThrow(/"sessions" must have a token/);
    expect(() => kv["other"]).toThrow(/"other" is malformed/);
  });

  test("the bootstrap envelope must carry an object of KV stores", () => {
    expect(() =>
      parseBootstrapEnvelope(JSON.stringify({ token: "t", secrets: {}, kv: [] })),
    ).toThrow("kv?: object");
    // Older servers omit it.
    expect(parseBootstrapEnvelope(JSON.stringify({ token: "t", secrets: {} })).kv).toBeUndefined();
  });
});

describe("TAKO_BOOTSTRAP_DATA without an entrypoint", () => {
  test("plain scripts read KV stores on first access", async () => {
    const index = new URL("../src/index.ts", import.meta.url).pathname;
    const script = `
      const { tako } = await import(${JSON.stringify(index)});
      console.log(JSON.stringify({
        store: tako.kv.sessions,
        leftover: process.env.TAKO_BOOTSTRAP_DATA ?? null,
      }));
    `;
    const child = Bun.spawn(["bun", "-e", script], {
      env: {
        ...process.env,
        TAKO_BOOTSTRAP_DATA: JSON.stringify({
          token: "",
          secrets: {},
          kv: { sessions: SESSIONS },
        }),
      },
      stdout: "pipe",
      stderr: "pipe",
    });
    const [code, out, err] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
    ]);
    expect(err).toBe("");
    expect(code).toBe(0);
    expect(JSON.parse(out)).toEqual({ store: { kv: "sessions" }, leftover: null });
  });
});
