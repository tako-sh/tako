import { beforeEach, describe, expect, test } from "bun:test";
import {
  getInternalToken,
  getStorageBindings,
  injectBootstrap,
  loadSecrets,
} from "../src/tako/secrets";
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
      storages: {
        uploads: {
          provider: "s3",
          bucket: "app-uploads",
          endpoint: "https://abc.r2.cloudflarestorage.com",
          region: "auto",
          access_key_id: "key-id",
          secret_access_key: "secret",
        },
      },
    });

    initBootstrapFromFd(() => envelope);

    expect(getInternalToken()).toBe("tok-abc");
    const secrets = loadSecrets();
    expect(secrets["DATABASE_URL"]).toBe("postgres://x");
    expect(secrets["API_KEY"]).toBe("sk-123");
    expect(getStorageBindings()["uploads"]).toMatchObject({ bucket: "app-uploads" });
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
