import { expect, test } from "bun:test";
import { Type as t, type TSchema } from "@sinclair/typebox";
import { createServiceClient, createServiceHandler, defineService } from "../../src/services";

async function check(schema: TSchema, wire: string, valid: boolean) {
  const value: unknown = JSON.parse(wire);
  let handlerCalls = 0;
  let fetchCalls = 0;
  const service = defineService("unique", {
    post: { method: "POST", path: "/", body: schema, responses: { 204: null } },
    get: { method: "GET", path: "/", responses: { 200: schema } },
  });
  const handler = createServiceHandler(service, {
    post: () => {
      handlerCalls++;
      return { status: 204, body: undefined };
    },
    get: () => ({ status: 200, body: value }),
  });
  const request = new Request("http://fixture/", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: wire,
  });
  expect((await handler(request)).status).toBe(valid ? 204 : 400);
  expect(handlerCalls).toBe(valid ? 1 : 0);
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async (_input, init) => {
      fetchCalls++;
      return init?.method === "POST"
        ? new Response(null, { status: 204 })
        : new Response(wire, { headers: { "content-type": "application/json" } });
    },
  });
  if (valid) {
    expect((await client.post({ body: value })).status).toBe(204);
    expect((await client.get()).body).toEqual(value);
    expect((await handler(new Request("http://fixture/"))).status).toBe(200);
    expect(fetchCalls).toBe(2);
  } else {
    await expect(client.post({ body: value })).rejects.toMatchObject({
      code: "validation",
      phase: "request",
    });
    expect(fetchCalls).toBe(0);
    await expect(client.get()).rejects.toMatchObject({ code: "validation", phase: "response" });
    await expect(handler(new Request("http://fixture/"))).rejects.toMatchObject({
      code: "validation",
      phase: "response",
    });
  }
}

test("uniqueItems uses JSON zero equality and distinguishes colliding TypeBox hashes", async () => {
  await check(t.Array(t.Number(), { uniqueItems: true }), "[-0,0]", false);
  await check(t.Array(t.Number(), { uniqueItems: true }), "[0,0]", false);
  await check(
    t.Array(t.String(), { uniqueItems: true }),
    JSON.stringify(["Ā", "\u0001\u0000"]),
    true,
  );
  await check(t.Array(t.String(), { uniqueItems: true }), '["same","same"]', false);
  await check(t.Array(t.Number(), { uniqueItems: false }), "[0,0]", true);
});

test("uniqueItems compares nested JSON structures independent of object key order", async () => {
  const item = t.Object({ a: t.Array(t.Number()), b: t.String() });
  const schema = t.Array(item, { uniqueItems: true, minItems: 1, maxItems: 2 });
  await check(schema, '[{"a":[0],"b":"x"},{"b":"x","a":[-0]}]', false);
  await check(schema, '[{"a":[0,1],"b":"x"},{"b":"x","a":[1,0]}]', true);
  await check(schema, '[{"a":[],"b":"x"},{"a":[],"b":"x","extra":true}]', true);
  await check(schema, "[]", false);
  await check(schema, '[{"a":[],"b":"x"},{"a":[],"b":"y"},{"a":[],"b":"z"}]', false);
  await check(t.Array(t.Array(t.String()), { uniqueItems: true }), '[["a"],["a","b"]]', true);
  await check(
    t.Array(t.Object({}), { uniqueItems: true }),
    '[{"__proto__":1},{"__proto__":2}]',
    true,
  );
  await check(
    t.Object({ nested: schema }),
    '{"nested":[{"a":[],"b":"x"},{"b":"x","a":[]}]}',
    false,
  );
});
