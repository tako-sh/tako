import { describe, expect, test } from "bun:test";
import { ChannelRegistry } from "../src/channels";
import { defineChannel } from "../src/channels/define";

function newRegistry() {
  return new ChannelRegistry();
}

describe("ChannelRegistry.register + resolve", () => {
  test("resolves exact registered channel names", () => {
    const reg = newRegistry();
    reg.register(
      "chat",
      defineChannel("chat", {
        auth: { verify: async () => true },
      }),
    );

    const hit = reg.resolve("chat");
    expect(hit?.definition.channel).toBe("chat");
    expect(reg.resolve("chat/r1")).toBeNull();
  });

  test("returns null for unmatched channel", () => {
    const reg = newRegistry();
    reg.register(
      "chat",
      defineChannel("chat", {
        auth: { verify: async () => true },
      }),
    );
    expect(reg.resolve("other")).toBeNull();
  });

  test("rejects duplicate channel names", () => {
    const reg = newRegistry();
    reg.register("status", defineChannel("status"));
    expect(() => reg.register("status", defineChannel("status"))).toThrow(/duplicate/);
  });
});

describe("ChannelRegistry.authorize", () => {
  test("rejects unknown channels", async () => {
    const reg = newRegistry();
    const resp = await reg.authorize({
      channel: "nope",
      operation: "subscribe",
      params: {},
    });
    expect(resp).toEqual({ ok: false });
  });

  test("allows public channels without verify", async () => {
    const reg = newRegistry();
    reg.register("status", defineChannel("status"));
    const resp = await reg.authorize({
      channel: "status",
      operation: "subscribe",
      params: {},
    });
    expect(resp.ok).toBe(true);
  });

  test("allows when verify returns true; stamps transport when handler present", async () => {
    const reg = newRegistry();
    reg.register(
      "chat",
      defineChannel("chat", {
        auth: { verify: async () => true },
        handler: { msg: async (data: { text: string }) => data },
      }),
    );
    const resp = await reg.authorize({
      channel: "chat",
      operation: "subscribe",
      params: { roomId: "r1" },
    });
    expect(resp.ok).toBe(true);
    expect(resp.transport).toBe("ws");
  });

  test("passes params and operation into verify callback", async () => {
    const reg = newRegistry();
    let seen: unknown = null;
    reg.register(
      "chat",
      defineChannel("chat", {
        auth: {
          verify: async (input) => {
            seen = { params: input.params, operation: input.operation, header: input.header };
            return true;
          },
        },
        handler: { msg: async (d) => d },
      }),
    );
    await reg.authorize({
      channel: "chat",
      operation: "publish",
      params: { roomId: "r1" },
      header: { scheme: "Bearer", value: "abc" },
    });
    expect(seen).toEqual({
      params: { roomId: "r1" },
      operation: "publish",
      header: { scheme: "Bearer", value: "abc" },
    });
  });

  test("rejects client publish on SSE channel (no handler)", async () => {
    const reg = newRegistry();
    reg.register(
      "status",
      defineChannel("status", {
        auth: { verify: async () => true },
      }),
    );
    const resp = await reg.authorize({
      channel: "status",
      operation: "publish",
      params: {},
    });
    expect(resp).toEqual({ ok: false, reason: "sse_publish_not_allowed" });
  });

  test("propagates subject from ChannelGrant verdict", async () => {
    const reg = newRegistry();
    reg.register(
      "status",
      defineChannel("status", {
        auth: { verify: async () => ({ subject: "user-42" }) },
      }),
    );
    const resp = await reg.authorize({
      channel: "status",
      operation: "subscribe",
      params: {},
    });
    expect(resp.ok).toBe(true);
    expect(resp.subject).toBe("user-42");
  });

  test("denies when verify returns false", async () => {
    const reg = newRegistry();
    reg.register(
      "private",
      defineChannel("private", {
        auth: { verify: async () => false },
      }),
    );
    const resp = await reg.authorize({
      channel: "private",
      operation: "subscribe",
      params: {},
    });
    expect(resp).toEqual({ ok: false });
  });
});
