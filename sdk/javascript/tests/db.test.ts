import { afterEach, describe, expect, test } from "bun:test";
import { inspect } from "node:util";
import { createDbBag, loadDb } from "../src/db";
import { injectBootstrap } from "../src/tako/secrets";
import { initBootstrapFromFd } from "../src/tako/secrets-fd";

const MAIN = { url: "http://127.0.0.1:9845", authToken: "tok-main" };

describe("tako.db", () => {
  afterEach(() => {
    injectBootstrap({ token: null, secrets: {} });
  });

  test("exposes connections that drivers accept", () => {
    injectBootstrap({ token: "t", secrets: {}, databases: { main: MAIN } });
    const databases = loadDb<{ main: unknown }>();
    expect(databases.main.url).toBe(MAIN.url);
    expect(databases.main.authToken).toBe(MAIN.authToken);
    // Drivers read the fields or spread the config.
    expect({ ...databases.main }).toEqual(MAIN);
    expect(Object.isFrozen(databases.main)).toBe(true);
    expect(databases.main).toBe(databases.main);
  });

  test("redacts the token when logged or serialized", () => {
    injectBootstrap({ token: "t", secrets: {}, databases: { main: MAIN } });
    const main = loadDb()["main"]!;
    expect(JSON.stringify(main)).not.toContain("tok-main");
    expect(inspect(main)).not.toContain("tok-main");
    expect(inspect(main)).toContain(MAIN.url);
  });

  test("reads bindings on access, after the bootstrap arrives", () => {
    const databases = loadDb();
    expect(databases["main"]).toBeUndefined();
    expect("main" in databases).toBe(false);
    injectBootstrap({ token: "t", secrets: {}, databases: { main: MAIN } });
    expect(databases["main"]?.url).toBe(MAIN.url);
    expect("main" in databases).toBe(true);
    expect(Reflect.ownKeys(databases)).toEqual(["main"]);
  });

  test("lists bindings like a plain object, without their tokens", () => {
    injectBootstrap({ token: "t", secrets: {}, databases: { main: MAIN } });
    const databases = loadDb();
    expect(Object.keys(databases)).toEqual(["main"]);
    expect(Object.entries(databases).map(([name, db]) => [name, db.url])).toEqual([
      ["main", MAIN.url],
    ]);
    expect(Object.keys({ ...databases })).toEqual(["main"]);
    expect(JSON.stringify(databases)).not.toContain("tok-main");
    expect(inspect(databases)).not.toContain("tok-main");
  });

  test("rejects malformed bindings with the binding name", () => {
    const databases = createDbBag(() => ({ main: { url: "http://x" } }));
    expect(() => databases["main"]).toThrow(/"main"/);
  });

  test("the bootstrap envelope must carry an object of databases", () => {
    const exit = process.exit;
    const error = console.error;
    let code: number | undefined;
    process.exit = ((value?: number) => {
      code = value;
      throw new Error("exit");
    }) as typeof process.exit;
    console.error = () => {};
    try {
      expect(() =>
        initBootstrapFromFd(() => JSON.stringify({ token: "t", secrets: {}, databases: [] })),
      ).toThrow("exit");
      expect(code).toBe(1);
    } finally {
      process.exit = exit;
      console.error = error;
    }
  });
});

describe("TAKO_BOOTSTRAP_DATA without an entrypoint", () => {
  test("plain scripts read secrets and databases on first access", async () => {
    const index = new URL("../src/index.ts", import.meta.url).pathname;
    const script = `
      const { tako } = await import(${JSON.stringify(index)});
      console.log(JSON.stringify({
        url: tako.db.main?.url,
        secret: tako.secrets.API_KEY,
        leftover: process.env.TAKO_BOOTSTRAP_DATA ?? null,
      }));
    `;
    const child = Bun.spawn(["bun", "-e", script], {
      env: {
        ...process.env,
        TAKO_BOOTSTRAP_DATA: JSON.stringify({
          token: "",
          secrets: { API_KEY: "key" },
          databases: { main: MAIN },
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
    expect(JSON.parse(out)).toEqual({ url: MAIN.url, secret: "key", leftover: null });
  });
});
