import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { Channel } from "../src/channels";
import { composeSdkHint, injectBootstrap, loadSecrets } from "../src/tako/secrets";
import { defineWorkflow, setWorkflowRuntime } from "../src/workflows/define";
import { expectAsyncToThrow } from "./assertions";

const HINT = "isn't marked `sdk: true` in x-tako";

describe("Compose services without sdk: true", () => {
  beforeEach(() => {
    process.env.TAKO_COMPOSE_SERVICE = "web";
    injectBootstrap({ token: null, secrets: {} });
    setWorkflowRuntime(null);
  });

  afterEach(() => {
    delete process.env.TAKO_COMPOSE_SERVICE;
    injectBootstrap({ token: null, secrets: {} });
  });

  test("import and startup do not fail", () => {
    const secrets = loadSecrets();
    expect(JSON.stringify(secrets)).toBe(JSON.stringify("[REDACTED]"));
    expect(Object.keys(secrets)).toEqual([]);
  });

  test("secrets, channels and workflows explain the missing marker on first use", async () => {
    expect(composeSdkHint()).toContain(HINT);
    expect(composeSdkHint()).toContain("services.web.x-tako");
    expect(() => loadSecrets<Record<string, string>>()["API_KEY"]).toThrow(HINT);
    const channel = new Channel("news");
    await expectAsyncToThrow(() => channel.publish({ type: "update", data: "x" }), HINT);
    await expectAsyncToThrow(() => channel.close(), HINT);
    const workflow = defineWorkflow("send", { handler: async () => {} });
    expect(() => workflow.enqueue({})).toThrow(HINT);
  });

  test("an sdk: true service with its bootstrap gets no hint", () => {
    injectBootstrap({ token: "group-token", secrets: { API_KEY: "secret" } });
    expect(composeSdkHint()).toBeNull();
    expect(loadSecrets<Record<string, string>>()["API_KEY"]).toBe("secret");
  });

  test("processes outside Compose keep their existing behavior", () => {
    delete process.env.TAKO_COMPOSE_SERVICE;
    expect(composeSdkHint()).toBeNull();
    expect(loadSecrets<Record<string, string>>()["API_KEY"]).toBeUndefined();
  });
});
