import { afterEach, describe, expect, test } from "bun:test";
import { Channel, setChannelSocketPublisher } from "../../src/channels";
import { defineChannel } from "../../src/channels/define";
import { channelStreamParams } from "../../src/channels/params";

afterEach(() => setChannelSocketPublisher(null));

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
