import { afterEach, describe, expect, test } from "bun:test";
import { inspect } from "node:util";
import { createKvBag, loadKv } from "../src/kv";
import { injectBootstrap, parseBootstrapEnvelope } from "../src/tako/secrets";

const SESSIONS = { url: "redis://:tok-sessions@127.0.0.1:9846" };
const REDACTED_URL = "redis://:[REDACTED]@127.0.0.1:9846";

describe("tako.kv", () => {
  afterEach(() => {
    injectBootstrap({ token: null, secrets: {} });
  });

  test("exposes connections that Redis clients accept", () => {
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    const kv = loadKv<{ sessions: unknown }>();
    expect(kv.sessions.url).toBe(SESSIONS.url);
    // Clients read the url or spread the config.
    expect({ ...kv.sessions }).toEqual(SESSIONS);
    expect(Object.isFrozen(kv.sessions)).toBe(true);
    expect(kv.sessions).toBe(kv.sessions);
  });

  test("redacts the token when logged or serialized", () => {
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    const sessions = loadKv()["sessions"]!;
    expect(JSON.parse(JSON.stringify(sessions))).toEqual({ url: REDACTED_URL });
    expect(inspect(sessions)).not.toContain("tok-sessions");
    expect(inspect(sessions)).toContain(REDACTED_URL);
  });

  test("redacts unexpected urls without leaking a part of the token", () => {
    const kv = createKvBag(() => ({ odd: { url: "tok-odd" }, at: { url: "redis://:a@b@host:1" } }));
    expect(JSON.stringify(kv["odd"])).not.toContain("tok-odd");
    expect(JSON.parse(JSON.stringify(kv["at"]))).toEqual({ url: "redis://:[REDACTED]@host:1" });
  });

  test("reads bindings on access, after the bootstrap arrives", () => {
    const kv = loadKv();
    expect(kv["sessions"]).toBeUndefined();
    expect("sessions" in kv).toBe(false);
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    expect(kv["sessions"]?.url).toBe(SESSIONS.url);
    expect("sessions" in kv).toBe(true);
    expect(Reflect.ownKeys(kv)).toEqual(["sessions"]);
  });

  test("lists bindings like a plain object, without their tokens", () => {
    injectBootstrap({ token: "t", secrets: {}, kv: { sessions: SESSIONS } });
    const kv = loadKv();
    expect(Object.keys(kv)).toEqual(["sessions"]);
    expect(Object.entries(kv).map(([name, store]) => [name, store.url])).toEqual([
      ["sessions", SESSIONS.url],
    ]);
    expect(Object.keys({ ...kv })).toEqual(["sessions"]);
    expect(JSON.stringify(kv)).not.toContain("tok-sessions");
    expect(inspect(kv)).not.toContain("tok-sessions");
  });

  test("rejects malformed bindings with the binding name", () => {
    const kv = createKvBag(() => ({ sessions: {}, other: "redis://x" }));
    expect(() => kv["sessions"]).toThrow(/"sessions" must have a url/);
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
        url: tako.kv.sessions?.url,
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
    expect(JSON.parse(out)).toEqual({ url: SESSIONS.url, leftover: null });
  });
});
