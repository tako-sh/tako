import { beforeEach, describe, expect, test } from "bun:test";
import { getInternalToken, injectBootstrap, loadSecrets } from "../src/tako/secrets";
import { initBootstrapFromFd } from "../src/tako/secrets-fd";

describe("initBootstrapFromFd", () => {
  beforeEach(() => {
    // Reset the module-level store between tests.
    injectBootstrap({ token: null, secrets: {} });
  });

  test("parses envelope and exposes token + secrets", () => {
    const envelope = JSON.stringify({
      token: "tok-abc",
      secrets: { DATABASE_URL: "postgres://x", API_KEY: "sk-123" },
    });

    initBootstrapFromFd(() => envelope);

    expect(getInternalToken()).toBe("tok-abc");
    const secrets = loadSecrets();
    expect(secrets["DATABASE_URL"]).toBe("postgres://x");
    expect(secrets["API_KEY"]).toBe("sk-123");
  });

  test("empty envelope (no Tako fd) leaves store empty", () => {
    initBootstrapFromFd(() => null);

    expect(getInternalToken()).toBeNull();
    const secrets = loadSecrets();
    expect(secrets["ANY"]).toBeUndefined();
  });

  test("envelope with empty secrets still has token", () => {
    const envelope = JSON.stringify({ token: "only-token", secrets: {} });

    initBootstrapFromFd(() => envelope);

    expect(getInternalToken()).toBe("only-token");
  });
});
