import { afterEach, describe, expect, test } from "bun:test";
import { Channel, setChannelSocketCloser, setChannelSocketPublisher } from "../../src/channels";
import { defineChannel } from "../../src/channels/define";
import { channelStreamParams } from "../../src/channels/params";

afterEach(() => {
  setChannelSocketPublisher(null);
  setChannelSocketCloser(null);
});

function subscribeQuery(handle: { subscribe: Channel["subscribe"] }): string {
  let url = "";
  handle.subscribe({
    baseUrl: "https://app.test",
    eventSourceFactory: (value) => {
      url = value;
      return {};
    },
  });
  return new URL(url).search;
}

async function publishedParams(handle: { publish: Channel["publish"] }): Promise<unknown> {
  let received: unknown;
  setChannelSocketPublisher(async (channel, message, params) => {
    received = params;
    return { id: "1", channel, ...message };
  });
  await handle.publish({ type: "msg", data: null });
  return received;
}

const board = defineChannel("board", {
  paramsSchema: (t) =>
    t.Object({
      room: t.String(),
      page: t.Optional(t.Integer()),
      ratio: t.Optional(t.Number()),
      live: t.Optional(t.Boolean()),
      tag: t.Optional(t.Union([t.Literal("a"), t.Literal("b")])),
    }),
});

describe("channel params select one stream for publish and subscribe", () => {
  test("publish sends the values a subscriber's query is coerced to", async () => {
    const handle = board({ room: "42", page: 2, ratio: 0.5, live: false, tag: "a" });
    expect(subscribeQuery(handle)).toBe("?room=42&page=2&ratio=0.5&live=false&tag=a");
    // `room` is a string property, so the numeric-looking value stays a string.
    expect(await publishedParams(handle)).toEqual({
      room: "42",
      page: 2,
      ratio: 0.5,
      live: false,
      tag: "a",
    });
  });

  test("values are coerced by declared type, not by their JavaScript type", async () => {
    const loose = board({ room: 42, page: "3", live: "true", ratio: 1.0 } as never);
    expect(subscribeQuery(loose)).toBe("?room=42&page=3&live=true&ratio=1");
    expect(await publishedParams(loose)).toEqual({ room: "42", page: 3, live: true, ratio: 1 });
  });

  test("undefined, null, and missing params name the same stream", async () => {
    const bindings = [
      board({ room: "r" }),
      board({ room: "r", page: undefined }),
      board({ room: "r", page: null } as never),
    ];
    for (const handle of bindings) {
      expect(subscribeQuery(handle)).toBe("?room=r");
      expect(await publishedParams(handle)).toEqual({ room: "r" });
    }
  });

  test("an unparameterized channel publishes without params", async () => {
    const status = defineChannel("status");
    expect(subscribeQuery(status)).toBe("");
    expect(await publishedParams(status)).toEqual({});
  });

  test("nested params are rejected when bound and when declared", () => {
    expect(() => board({ room: { id: "r" } } as never)).toThrow(
      "Channel param 'room' has type object. Channel params must be flat",
    );
    expect(() =>
      new Channel("raw", undefined, { ids: ["a"] }).subscribe({ baseUrl: "https://app.test" }),
    ).toThrow("Channel param 'ids' is an array");
    expect(() =>
      defineChannel("nested", {
        paramsSchema: (t) => t.Object({ filter: t.Object({ room: t.String() }) }),
      }),
    ).toThrow("Channel 'nested' param 'filter' is declared as object");
    expect(() =>
      defineChannel("list", { paramsSchema: (t) => t.Object({ ids: t.Array(t.String()) }) }),
    ).toThrow("param 'ids' is declared as array");
  });

  test("untyped handles publish scalars as given", async () => {
    expect(channelStreamParams({ a: 1, b: "x", c: true, d: undefined, e: 3n })).toEqual({
      a: 1,
      b: "x",
      c: true,
      e: 3,
    });
    expect(() => channelStreamParams({ n: Number.NaN })).toThrow("not a finite number");
    expect(await publishedParams(new Channel("raw", undefined, { page: 2 }))).toEqual({ page: 2 });
  });
});

describe("params a JavaScript number cannot carry exactly are rejected on both sides", () => {
  const job = defineChannel("job", {
    paramsSchema: (t) => t.Object({ id: t.Integer(), ratio: t.Optional(t.Number()) }),
  });
  const schema = job.definition.paramsSchema;
  // Subscribe query values tako-server parses as exact integers.
  const unsafe = [
    "9007199254740992", // 2^53: 2^53 + 1 rounds to it
    "9007199254740993", // 2^53 + 1
    "1234567890123456789", // snowflake id
    "9223372036854775808", // 2^63, beyond i64
    "-9007199254740992",
    "1".repeat(400),
  ];

  test("safe integers at the bounds bind identically for subscribe and publish", async () => {
    for (const raw of ["9007199254740991", "-9007199254740991", "0", "+7"]) {
      for (const id of [raw, BigInt(raw), Number(raw)]) {
        const handle = job({ id } as never);
        const query = new URLSearchParams(subscribeQuery(handle)).get("id")!;
        const published = (await publishedParams(handle)) as { id: number };
        // What the server parses from the query is what publish sends.
        expect(BigInt(published.id)).toBe(BigInt(query));
      }
    }
  });

  test("unsafe integers fail at bind time, subscribe, publish, and close", async () => {
    let closed = false;
    setChannelSocketPublisher(async (channel, message) => ({ id: "1", channel, ...message }));
    setChannelSocketCloser(async (channel) => {
      closed = true;
      return { id: "1", channel, type: "tako.close", data: null };
    });
    for (const raw of unsafe) {
      const inputs: unknown[] = [raw];
      if (!raw.startsWith("1111")) inputs.push(BigInt(raw), Number(raw));
      for (const id of inputs) {
        expect(() => job({ id } as never)).toThrow("Declare large identifiers");
        const untyped = new Channel("job", undefined, { id }, undefined, schema);
        expect(() => untyped.subscribe({ baseUrl: "https://app.test" })).toThrow(
          "outside the safe integer range",
        );
        await expect(untyped.publish({ type: "msg", data: null })).rejects.toThrow(
          "outside the safe integer range",
        );
        await expect(untyped.close()).rejects.toThrow("outside the safe integer range");
      }
    }
    expect(closed).toBe(false);
    // Without a schema, a bigint has no exact number either.
    expect(() => channelStreamParams({ id: 2n ** 63n })).toThrow("Declare large identifiers");
  });

  test("string params carry large identifiers exactly", async () => {
    const order = defineChannel("order", { paramsSchema: (t) => t.Object({ id: t.String() }) });
    const handle = order({ id: "1234567890123456789" });
    expect(subscribeQuery(handle)).toBe("?id=1234567890123456789");
    expect(await publishedParams(handle)).toEqual({ id: "1234567890123456789" });
  });

  test("out-of-range numerals never publish without their param", () => {
    for (const ratio of ["1e400", "-1e400"]) {
      expect(() => job({ id: 1, ratio } as never)).toThrow("not a finite number");
      expect(() => channelStreamParams({ id: 1, ratio }, schema)).toThrow("not a finite number");
    }
    expect(() => channelStreamParams({ id: "9".repeat(400) }, schema)).toThrow(
      "outside the safe integer range",
    );
  });
});
