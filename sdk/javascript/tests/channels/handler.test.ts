import { describe, expect, test } from "bun:test";
import { ChannelRegistry } from "../../src/channels";
import { defineChannel } from "../../src/channels/define";
import { dispatchWsMessage } from "../../src/channels/handler";

function makeRegistry() {
  const reg = new ChannelRegistry();
  reg.register(
    "chat",
    defineChannel("chat", {
      auth: { verify: async () => ({ subject: "u1" }) },
      handler: {
        msg: async (data: { text: string }, ctx) =>
          ({
            text: data.text.toUpperCase(),
            roomId: ctx.params.roomId,
          }) as unknown as { text: string },
        typing: async () => undefined,
      },
    }),
  );
  return reg;
}

describe("dispatchWsMessage", () => {
  test("runs the handler for the message type and returns fanout data", async () => {
    const reg = makeRegistry();
    const res = await dispatchWsMessage(reg, {
      channel: "chat",
      params: { roomId: "r1" },
      frame: { type: "msg", data: { text: "hello" } },
      subject: "u1",
    });
    expect(res.action).toBe("fanout");
    if (res.action !== "fanout") throw new Error("unexpected");
    expect(res.data).toEqual({ text: "HELLO", roomId: "r1" });
  });

  test("drops the message when handler returns undefined", async () => {
    const reg = makeRegistry();
    const res = await dispatchWsMessage(reg, {
      channel: "chat",
      params: { roomId: "r1" },
      frame: { type: "typing", data: { userId: "u1" } },
      subject: "u1",
    });
    expect(res.action).toBe("drop");
  });

  test("passes message through when no handler registered for type", async () => {
    const reg = makeRegistry();
    const res = await dispatchWsMessage(reg, {
      channel: "chat",
      params: { roomId: "r1" },
      frame: { type: "joined", data: { userId: "u2" } },
      subject: "u1",
    });
    expect(res.action).toBe("fanout");
    if (res.action !== "fanout") throw new Error("unexpected");
    expect(res.data).toEqual({ userId: "u2" });
  });

  test("rejects when channel does not match a definition", async () => {
    const reg = makeRegistry();
    const res = await dispatchWsMessage(reg, {
      channel: "unknown",
      frame: { type: "msg", data: {} },
      subject: "u1",
    });
    expect(res.action).toBe("reject");
  });

  test("rejects SSE channels (no handler on definition)", async () => {
    const reg = new ChannelRegistry();
    reg.register("status", defineChannel("status"));
    const res = await dispatchWsMessage(reg, {
      channel: "status",
      frame: { type: "ping", data: {} },
      subject: "u1",
    });
    expect(res.action).toBe("reject");
    if (res.action !== "reject") throw new Error("unexpected");
    expect(res.reason).toBe("sse_channel_not_writable");
  });

  test("drops and reports error when handler throws", async () => {
    const reg = new ChannelRegistry();
    reg.register(
      "boom",
      defineChannel("boom", {
        auth: { verify: async () => true },
        handler: {
          msg: async () => {
            throw new Error("kaboom");
          },
        },
      }),
    );
    const res = await dispatchWsMessage(reg, {
      channel: "boom",
      params: { id: "1" },
      frame: { type: "msg", data: { x: 1 } },
      subject: "u1",
    });
    expect(res.action).toBe("drop");
    if (res.action !== "drop") throw new Error("unexpected");
    expect(res.error).toMatch(/kaboom/);
  });
});
