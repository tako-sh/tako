import { expect, test } from "bun:test";
import { Type as t, type TSchema } from "@sinclair/typebox";
import { createServiceClient } from "../../src/services/client";
import { defineService } from "../../src/services/contract";
import { createServiceHandler } from "../../src/services/handler";

async function roundtrip(schema: TSchema, value: unknown, valid: boolean) {
  const service = defineService("constraints", {
    post: { method: "POST", path: "/", body: schema, responses: { 200: schema } },
  });
  const handle = createServiceHandler(service, { post: ({ body }) => ({ status: 200, body }) });
  const raw = new Request("http://fixture/", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(value),
  });
  expect((await handle(raw)).status).toBe(valid ? 200 : 400);
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: (input, init) => handle(new Request(input, init)),
  });
  if (valid) expect((await client.post({ body: value })).body).toEqual(value);
  else
    await expect(client.post({ body: value })).rejects.toMatchObject({
      code: "validation",
      phase: "request",
    });
  const reader = defineService("reader", {
    get: { method: "GET", path: "/", responses: { 200: schema } },
  });
  const responseClient = createServiceClient(reader, {
    baseURL: "http://fixture",
    fetch: async () => Response.json(value),
  });
  if (valid) expect((await responseClient.get()).body).toEqual(value);
  else
    await expect(responseClient.get()).rejects.toMatchObject({
      code: "validation",
      phase: "response",
    });
  const output = createServiceHandler(reader, { get: () => ({ status: 200, body: value }) });
  if (valid) expect((await output(new Request("http://fixture/"))).status).toBe(200);
  else
    await expect(output(new Request("http://fixture/"))).rejects.toMatchObject({
      code: "validation",
      phase: "response",
    });
}

test("JSON Schema lengths count Unicode code points at all four boundaries", async () => {
  await roundtrip(t.String({ minLength: 2 }), "😀", false);
  await roundtrip(t.String({ maxLength: 1 }), "😀", true);
  await roundtrip(t.String({ minLength: 2, maxLength: 2 }), "😀a", true);
  await roundtrip(t.String({ maxLength: 1 }), "😀a", false);
});

test("decimal multipleOf accepts exact decimal multiples and rejects nonmultiples", async () => {
  await roundtrip(t.Number({ multipleOf: 0.1 }), 0.3, true);
  await roundtrip(t.Number({ multipleOf: 0.1 }), -0.3, true);
  await roundtrip(t.Number({ multipleOf: 0.1 }), 0.31, false);
  await roundtrip(t.Number({ multipleOf: 0.001 }), 0.007, true);
  await roundtrip(t.Number({ multipleOf: 0.1 }), 0.300000001, false);
});

test("standard scalar constraints compose in nested arrays, objects, unions and intersections", async () => {
  const schema = t.Object({
    values: t.Array(t.Union([t.String({ minLength: 2 }), t.Number({ multipleOf: 0.1 })])),
  });
  await roundtrip(schema, { values: ["😀", 0.3] }, false);
  await roundtrip(schema, { values: ["😀a", 0.3] }, true);
  await roundtrip(
    t.Intersect([
      t.Object({ value: t.String({ maxLength: 1 }) }),
      t.Object({ number: t.Number({ multipleOf: 0.1 }) }),
    ]),
    { value: "😀", number: 0.3 },
    true,
  );
});
