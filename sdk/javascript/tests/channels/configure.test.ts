import { describe, expect, test } from "bun:test";
import {
  configureChannels,
  getChannelsConfig,
  resetChannelsConfig,
} from "../../src/channels/configure";
import { expectAsyncToThrow } from "../assertions";

describe("configureChannels", () => {
  test("token resolver is invoked per call", async () => {
    let count = 0;
    configureChannels({ token: () => `t-${++count}` });

    expect(await getChannelsConfig().resolveToken()).toBe("t-1");
    expect(await getChannelsConfig().resolveToken()).toBe("t-2");

    resetChannelsConfig();
  });

  test("fetch and websocket overrides are honored", () => {
    const fetchOverride = (() => {}) as unknown as typeof fetch;
    const websocketOverride = class {} as unknown as typeof WebSocket;

    configureChannels({ fetch: fetchOverride, websocket: websocketOverride });

    expect(getChannelsConfig().fetch).toBe(fetchOverride);
    expect(getChannelsConfig().websocket).toBe(websocketOverride);

    resetChannelsConfig();
  });

  test("missing token throws actionable error", async () => {
    resetChannelsConfig();

    await expectAsyncToThrow(
      () => getChannelsConfig().resolveToken(),
      /configureChannels\(\{ token \}\)/,
    );
  });

  test("optional token resolves null when unset", async () => {
    resetChannelsConfig();

    expect(await getChannelsConfig().resolveOptionalToken()).toBeNull();
  });

  test("null token throws actionable error", async () => {
    configureChannels({ token: () => null });

    await expectAsyncToThrow(() => getChannelsConfig().resolveToken(), /returned null/);

    resetChannelsConfig();
  });
});
