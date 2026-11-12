import { describe, expect, test } from "bun:test";
import type { Type } from "@sinclair/typebox";
import { Value } from "@sinclair/typebox/value";
import { setChannelSocketPublisher } from "../../src/channels";
import {
  CHANNEL_SYMBOL,
  defineChannel,
  isChannelDefinition,
  isChannelExport,
} from "../../src/channels/define";
import { definePresence } from "../../src/presence/define";

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

describe("defineChannel params schemas declare every param", () => {
  test("channel and presence schemas reject undeclared params unless they opt in", () => {
    expect(defineChannel("status").definition.paramsSchema).toMatchObject({
      additionalProperties: false,
    });
    const room = defineChannel("room", { paramsSchema: (t) => t.Object({ id: t.String() }) });
    expect(room.definition.paramsSchema).toMatchObject({ additionalProperties: false });
    expect(Value.Check(room.definition.paramsSchema, { id: "a", junk: "1" })).toBe(false);
    const open = defineChannel("open", {
      paramsSchema: (t) => t.Object({ id: t.String() }, { additionalProperties: true }),
    });
    expect(open.definition.paramsSchema).toMatchObject({ additionalProperties: true });
    // Store scopes carry JSON params and keep their schema as declared.
    expect(defineChannel("tako.store:notes").definition.paramsSchema).not.toHaveProperty(
      "additionalProperties",
    );
  });
});

type T = typeof Type;

describe("defineChannel params schemas are one flat object", () => {
  test("combinators and references are rejected at definition time", () => {
    const a = (t: T) => t.Object({ a: t.String() });
    const b = (t: T) => t.Object({ b: t.String() });
    for (const [keyword, paramsSchema] of [
      ["allOf", (t: T) => t.Intersect([a(t), b(t)])],
      ["anyOf", (t: T) => t.Union([a(t), b(t)])],
      ["$ref", (t: T) => t.Ref("Room")],
    ] as const) {
      expect(() => defineChannel("room", { paramsSchema })).toThrow(
        keyword === "$ref"
          ? `Channel 'room' params schema uses '${keyword}'`
          : `Channel 'room' params schema declares params through 'properties' inside '${keyword}'`,
      );
      expect(() => definePresence("room", { paramsSchema })).toThrow("t.Composite");
    }
    expect(() => defineChannel("room", { paramsSchema: (t) => t.String() })).toThrow(
      `params schema has type "string"`,
    );
    // Composite flattens objects into one, and property-level unions are fine.
    const room = defineChannel("room", {
      paramsSchema: (t) =>
        t.Composite([a(t), t.Object({ kind: t.Union([t.Literal("x"), t.Literal("y")]) })]),
    });
    expect(Object.keys(room.definition.paramsSchema.properties)).toEqual(["a", "kind"]);
    // Store scopes carry JSON params and keep their schema as declared.
    expect(() =>
      defineChannel("tako.store:notes", { paramsSchema: (t) => t.Union([a(t), b(t)]) }),
    ).not.toThrow();
  });

  test("top-level combinators may constrain the declared params", () => {
    const ids = (t: T, options: object) =>
      t.Object({ userId: t.Optional(t.String()), teamId: t.Optional(t.String()) }, options);
    for (const constraint of [
      { oneOf: [{ required: ["userId"] }, { required: ["teamId"] }] },
      { if: { required: ["userId"] }, then: { required: ["teamId"] } },
      { not: { required: ["teamId"] } },
      { dependentSchemas: { teamId: { required: ["userId"] } } },
      { allOf: [{ anyOf: [{ required: ["userId"] }, { additionalProperties: false }] }] },
    ]) {
      const scoped = defineChannel("scoped", { paramsSchema: (t) => ids(t, constraint) });
      const presence = definePresence("scoped", { paramsSchema: (t) => ids(t, constraint) });
      // The constraint reaches tako-server, which enforces it on every request.
      for (const definition of [scoped.definition, presence.definition]) {
        expect(definition.paramsSchema).toMatchObject({
          ...constraint,
          additionalProperties: false,
        });
        expect(Object.keys(definition.paramsSchema.properties)).toEqual(["userId", "teamId"]);
      }
    }
  });

  test("a combinator that declares params is rejected wherever it nests", () => {
    for (const [keyword, combinator, constraint] of [
      ["properties", "allOf", { allOf: [{ properties: { teamId: { type: "string" } } }] }],
      [
        "properties",
        "anyOf",
        { anyOf: [{ allOf: [{ not: { properties: { teamId: { type: "string" } } } }] }] },
      ],
      ["additionalProperties", "oneOf", { oneOf: [{ additionalProperties: true }] }],
      ["additionalProperties", "if", { if: { additionalProperties: { type: "string" } } }],
      ["$ref", "not", { not: { $ref: "#/$defs/x" } }],
      [
        "patternProperties",
        "dependentSchemas",
        { dependentSchemas: { userId: { patternProperties: { "^t": {} } } } },
      ],
    ] as const) {
      const paramsSchema = (t: T) => t.Object({ userId: t.String() }, constraint);
      for (const define of [
        () => defineChannel("room", { paramsSchema }),
        () => definePresence("room", { paramsSchema }),
      ]) {
        expect(define).toThrow(`declares params through '${keyword}' inside '${combinator}'`);
      }
    }
    expect(() =>
      defineChannel("room", {
        paramsSchema: (t) => t.Object({ a: t.String() }, { patternProperties: { "^b": {} } }),
      }),
    ).toThrow("params schema uses 'patternProperties'");
  });
});
