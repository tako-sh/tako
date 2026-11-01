import { describe, expect, test } from "bun:test";
import { setChannelSocketPublisher } from "../../src/channels";
import {
  CHANNEL_SYMBOL,
  defineChannel,
  isChannelDefinition,
  isChannelExport,
} from "../../src/channels/define";

describe("defineChannel", () => {
  test("server publishes include lifecycle before any subscription", async () => {
    let received: unknown;
    setChannelSocketPublisher(async (channel, payload) => {
      received = payload;
      return { id: "1", channel, ...payload };
    });
    try {
      const events = defineChannel("events", {
        replayWindowMs: 500,
        inactivityTtlMs: 1000,
      });
      await events.publish({ type: "message", data: "hello" });
      expect(received).toEqual({
        type: "message",
        data: "hello",
        lifecycle: {
          replayWindowMs: 500,
          inactivityTtlMs: 1000,
          keepaliveIntervalMs: 25000,
          maxConnectionLifetimeMs: 7200000,
        },
      });
    } finally {
      setChannelSocketPublisher(null);
    }
  });
  test("accepts name first", () => {
    const exp = defineChannel("status");
    expect(exp.definition.type).toBe(CHANNEL_SYMBOL);
    expect(exp.definition.channel).toBe("status");
    expect(exp.definition.auth).toBe(false);
  });

  test("accepts name first with params and WebSocket transport", () => {
    const exp = defineChannel("chat", {
      paramsSchema: (t) => t.Object({ roomId: t.String() }),
      transport: "ws",
    });
    expect(exp.definition.channel).toBe("chat");
    expect(exp.definition.transport).toBe("ws");
    expect(exp({ roomId: "r1" }).name).toBe("chat?roomId=r1");
  });

  test("public channel without auth", () => {
    const exp = defineChannel("status");
    expect(exp.definition.type).toBe(CHANNEL_SYMBOL);
    expect(exp.definition.channel).toBe("status");
    expect(exp.definition.auth).toBe(false);
    expect(exp.definition.paramsSchema).toMatchObject({ type: "object" });
    expect(exp.definition.transport).toBeUndefined();
  });

  test("serializes paramsSchema to JSON Schema", () => {
    const exp = defineChannel("chat", {
      paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
    });
    expect(exp.definition.paramsSchema).toMatchObject({
      type: "object",
      properties: { roomId: { type: "string", minLength: 1 } },
      required: ["roomId"],
    });
  });

  test("declarative auth defaults headerName to authorization", () => {
    const exp = defineChannel("private", {
      auth: { verify: () => true },
    });
    expect(exp.definition.auth).toMatchObject({ headerName: "authorization" });
  });

  test("auth headerName false disables header", () => {
    const exp = defineChannel("private", {
      auth: { headerName: false, cookieName: "session", verify: () => true },
    });
    expect(exp.definition.auth).toMatchObject({
      headerName: false,
      cookieName: "session",
    });
  });

  test("explicit transport enables WebSocket connections", () => {
    const exp = defineChannel("chat", {
      transport: "ws",
    }).$messageTypes<{ "chat.send": { text: string } }>();
    expect(exp.definition.transport).toBe("ws");
  });

  test("passes through lifecycle config", () => {
    const exp = defineChannel("status", {
      replayWindowMs: 1000,
      inactivityTtlMs: 2000,
      keepaliveIntervalMs: 3000,
      maxConnectionLifetimeMs: 4000,
    });
    expect(exp.definition.replayWindowMs).toBe(1000);
    expect(exp.definition.inactivityTtlMs).toBe(2000);
    expect(exp.definition.keepaliveIntervalMs).toBe(3000);
    expect(exp.definition.maxConnectionLifetimeMs).toBe(4000);
  });

  test("export is a typed handle when params absent", () => {
    const exp = defineChannel("status").$messageTypes<{
      ping: { at: number };
    }>();
    expect(exp.name).toBe("status");
    expect(typeof exp.publish).toBe("function");
    expect(isChannelExport(exp)).toBe(true);
  });

  test("export is callable when params present", () => {
    const exp = defineChannel("chat", {
      paramsSchema: (t) => t.Object({ roomId: t.String() }),
    });
    const handle = exp({ roomId: "r1" });
    expect(handle.name).toBe("chat?roomId=r1");
  });
});

describe("isChannelExport", () => {
  test("true for output of defineChannel", () => {
    expect(isChannelExport(defineChannel("status"))).toBe(true);
  });

  test("false for plain objects and bare definitions", () => {
    expect(isChannelExport({ auth: false })).toBe(false);
    expect(isChannelExport(null)).toBe(false);
    expect(isChannelExport(undefined)).toBe(false);
    expect(isChannelExport("string")).toBe(false);
  });
});

describe("isChannelDefinition", () => {
  test("true for the inner definition of a defineChannel result", () => {
    expect(isChannelDefinition(defineChannel("status").definition)).toBe(true);
  });

  test("false for plain objects", () => {
    expect(isChannelDefinition({ auth: false })).toBe(false);
    expect(isChannelDefinition(null)).toBe(false);
    expect(isChannelDefinition(undefined)).toBe(false);
    expect(isChannelDefinition("string")).toBe(false);
  });
});

test("ephemeral publishes carry the flag without a replay window", async () => {
  let received: unknown;
  setChannelSocketPublisher(async (channel, payload) => {
    received = payload;
    return { id: "", channel, ...payload };
  });
  try {
    const live = defineChannel("live", { ephemeral: true });
    expect(live.definition.ephemeral).toBe(true);
    await live.publish({ type: "progress", data: 42 });
    expect(received).toEqual({
      type: "progress",
      data: 42,
      lifecycle: {
        ephemeral: true,
        inactivityTtlMs: 0,
        keepaliveIntervalMs: 25000,
        maxConnectionLifetimeMs: 7200000,
      },
    });
    for (const replayWindowMs of [0, 100, 600000]) {
      expect(() => defineChannel("invalid", { ephemeral: true, replayWindowMs })).toThrow(
        "cannot set replayWindowMs",
      );
    }
    expect(() => defineChannel("durable", { ephemeral: false, replayWindowMs: 100 })).not.toThrow();
  } finally {
    setChannelSocketPublisher(null);
  }
});
