import { expect, test } from "bun:test";
import { Type as t } from "@sinclair/typebox";
import { defineService } from "../../src/services/contract";
import { createServiceClient } from "../../src/services/client";
import { createServiceHandler } from "../../src/services/handler";

const record = t.Object({ ["__proto__"]: t.Object({}) }, { additionalProperties: false });
const service = defineService("own-fields", {
  post: {
    method: "POST",
    path: "/",
    body: t.Object({ records: t.Array(record) }),
    responses: { 200: t.Object({ records: t.Array(record) }) },
  },
});

test("prototype fields cannot satisfy required JSON properties on either incoming boundary", async () => {
  let calls = 0;
  const handle = createServiceHandler(service, {
    post: ({ body }) => {
      calls++;
      return { status: 200, body };
    },
  });
  for (const payload of ['{"records":[{}]}', '{"records":[{"unlisted":true}]}']) {
    expect(
      (
        await handle(
          new Request("http://fixture/", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: payload,
          }),
        )
      ).status,
    ).toBe(400);
    const client = createServiceClient(service, {
      baseURL: "http://fixture",
      fetch: async () => new Response(payload, { headers: { "content-type": "application/json" } }),
    });
    await expect(client.post({ body: { records: [{ ["__proto__"]: {} }] } })).rejects.toMatchObject(
      { code: "validation", phase: "response" },
    );
  }
  expect(calls).toBe(0);
});

test("outgoing JavaScript values require own fields without mutating callers", async () => {
  const inherited: { records: { __proto__: object }[] } = {
    records: [Object.create({ ["__proto__"]: {} }) as { __proto__: object }],
  };
  let calls = 0;
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async () => {
      calls++;
      return Response.json({});
    },
  });
  await expect(client.post({ body: inherited })).rejects.toMatchObject({
    code: "validation",
    phase: "request",
  });
  expect(calls).toBe(0);
  expect(Object.getPrototypeOf(inherited.records[0])).not.toBeNull();
  const handle = createServiceHandler(service, { post: () => ({ status: 200, body: inherited }) });
  await expect(
    handle(
      new Request("http://fixture/", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: '{"records":[{"__proto__":{}}]}',
      }),
    ),
  ).rejects.toMatchObject({ code: "validation", phase: "response" });
});

test("legitimate own prototype-named JSON fields survive without exposing prototypes", async () => {
  const handle = createServiceHandler(service, {
    post: ({ body }) => {
      expect(Object.getPrototypeOf(body)).toBeNull();
      expect(Object.getPrototypeOf(body.records[0])).toBeNull();
      expect(Object.hasOwn(body.records[0]!, "__proto__")).toBe(true);
      return { status: 200, body };
    },
  });
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: (input, init) => handle(new Request(input, init)),
  });
  const result = await client.post({ body: { records: [{ ["__proto__"]: {} }] } });
  expect(Object.hasOwn(result.body.records[0]!, "__proto__")).toBe(true);
  expect(Object.getPrototypeOf(result.body.records[0])).toBeNull();
  expect(Object.getPrototypeOf(result.body.records[0]!.__proto__)).toBeNull();
  expect(JSON.stringify(result.body)).toBe('{"records":[{"__proto__":{}}]}');
});
