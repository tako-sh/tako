import { describe, expect, test } from "bun:test";
import { ChannelRegistry } from "../src/channels";
import { defineChannel } from "../src/channels/define";

function newRegistry() {
  return new ChannelRegistry();
}

describe("ChannelRegistry.register + resolve", () => {
  test("literal pattern beats param at the same position", () => {
    const reg = newRegistry();
    reg.register("lobby", defineChannel("chat/lobby", { auth: async () => true }));
    reg.register("chat", defineChannel("chat/:roomId", { auth: async () => true }));

    const lobbyHit = reg.resolve("chat/lobby");
    expect(lobbyHit?.params).toEqual({});
    expect(lobbyHit?.definition.pattern).toBe("chat/lobby");

    const paramHit = reg.resolve("chat/abc-123");
    expect(paramHit?.params).toEqual({ roomId: "abc-123" });
    expect(paramHit?.definition.pattern).toBe("chat/:roomId");
  });

  test("returns null for unmatched channel", () => {
    const reg = newRegistry();
    reg.register("chat", defineChannel("chat/:roomId", { auth: async () => true }));
    expect(reg.resolve("other/123")).toBeNull();
  });

  test("rejects duplicate patterns", () => {
    const reg = newRegistry();
    reg.register("a", defineChannel("status", { auth: async () => true }));
    expect(() => reg.register("b", defineChannel("status", { auth: async () => true }))).toThrow(
      /duplicate/,
    );
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

  test("allows when auth returns true; stamps transport when handler present", async () => {
    const reg = newRegistry();
    reg.register(
      "chat",
      defineChannel<{ msg: { text: string } }>("chat/:roomId", {
        auth: async () => true,
        handler: { msg: async (data) => data },
      }),
    );
    const resp = await reg.authorize({
      channel: "chat/r1",
      operation: "subscribe",
      params: { roomId: "r1" },
    });
    expect(resp.ok).toBe(true);
    expect(resp.transport).toBe("ws");
  });

  test("omits transport when no handler (SSE channel)", async () => {
    const reg = newRegistry();
    reg.register("status", defineChannel("status", { auth: async () => true }));
    const resp = await reg.authorize({
      channel: "status",
      operation: "subscribe",
      params: {},
    });
    expect(resp.ok).toBe(true);
    expect(resp.transport).toBeUndefined();
  });

  test("passes params and operation into auth callback", async () => {
    const reg = newRegistry();
    let seen: unknown = null;
    reg.register(
      "chat",
      defineChannel<{ msg: { text: string } }>("chat/:roomId", {
        auth: async (_req, ctx) => {
          seen = { params: ctx.params, operation: ctx.operation, pattern: ctx.pattern };
          return true;
        },
        handler: { msg: async (d) => d },
      }),
    );
    await reg.authorize({
      channel: "chat/r1",
      operation: "publish",
      params: { roomId: "r1" },
    });
    expect(seen).toEqual({
      params: { roomId: "r1" },
      operation: "publish",
      pattern: "chat/:roomId",
    });
  });

  test("rejects client publish on SSE channel (no handler)", async () => {
    const reg = newRegistry();
    reg.register("status", defineChannel("status", { auth: async () => true }));
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
        auth: async () => ({ subject: "user-42" }),
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

  test("denies when auth returns false", async () => {
    const reg = newRegistry();
    reg.register("private", defineChannel("private", { auth: async () => false }));
    const resp = await reg.authorize({
      channel: "private",
      operation: "subscribe",
      params: {},
    });
    expect(resp).toEqual({ ok: false });
  });
});
