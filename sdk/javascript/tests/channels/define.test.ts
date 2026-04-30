import { describe, expect, test } from "bun:test";
import {
  CHANNEL_SYMBOL,
  bindChannelName,
  defineChannel,
  isChannelDefinition,
  isChannelExport,
} from "../../src/channels/define";

describe("defineChannel", () => {
  test("public channel without auth", () => {
    const exp = defineChannel();
    expect(exp.definition.type).toBe(CHANNEL_SYMBOL);
    expect(exp.definition.auth).toBe(false);
    expect(exp.definition.paramsSchema).toMatchObject({ type: "object" });
    expect(exp.definition.handler).toBeUndefined();
  });

  test("serializes paramsSchema to JSON Schema", () => {
    const exp = defineChannel({
      paramsSchema: (t) => t.Object({ roomId: t.String({ minLength: 1 }) }),
    });
    expect(exp.definition.paramsSchema).toMatchObject({
      type: "object",
      properties: { roomId: { type: "string", minLength: 1 } },
      required: ["roomId"],
    });
  });

  test("declarative auth defaults headerName to authorization", () => {
    const exp = defineChannel({
      auth: { verify: () => true },
    });
    expect(exp.definition.auth).toMatchObject({ headerName: "authorization" });
  });

  test("auth headerName false disables header", () => {
    const exp = defineChannel({
      auth: { headerName: false, cookieName: "session", verify: () => true },
    });
    expect(exp.definition.auth).toMatchObject({
      headerName: false,
      cookieName: "session",
    });
  });

  test("handler presence implies ws transport", () => {
    const exp = defineChannel({
      handler: { "chat.send": async (data) => data },
    }).$messageTypes<{ "chat.send": { text: string } }>();
    expect(exp.definition.transport).toBe("ws");
  });

  test("passes through lifecycle config", () => {
    const exp = defineChannel({
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
    const exp = defineChannel().$messageTypes<{ ping: { at: number } }>();
    bindChannelName(exp.definition, "status");
    expect(exp.name).toBe("status");
    expect(typeof exp.publish).toBe("function");
    expect(isChannelExport(exp)).toBe(true);
  });

  test("export is callable when params present", () => {
    const exp = defineChannel({
      paramsSchema: (t) => t.Object({ roomId: t.String() }),
    });
    bindChannelName(exp.definition, "chat");
    const handle = exp({ roomId: "r1" });
    expect(handle.name).toBe("chat?roomId=r1");
  });
});

describe("isChannelExport", () => {
  test("true for output of defineChannel", () => {
    expect(isChannelExport(defineChannel())).toBe(true);
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
    expect(isChannelDefinition(defineChannel().definition)).toBe(true);
  });

  test("false for plain objects", () => {
    expect(isChannelDefinition({ auth: false })).toBe(false);
    expect(isChannelDefinition(null)).toBe(false);
    expect(isChannelDefinition(undefined)).toBe(false);
    expect(isChannelDefinition("string")).toBe(false);
  });
});
