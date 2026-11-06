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
  for (const value of [1e-13, -1e-13, Number.MIN_VALUE, 0.9999999999999999])
    await roundtrip(t.Number({ multipleOf: 1 }), value, false);
  await roundtrip(t.Number({ multipleOf: 1e-14 }), 3e-14, true);
  await roundtrip(t.Number({ multipleOf: 1e-14 }), 3.1e-14, false);
  await roundtrip(t.Number({ multipleOf: 1e20 }), 3e21, true);
  await roundtrip(t.Number({ multipleOf: 1e20 }), 3.01e21, false);
  await roundtrip(t.Number({ multipleOf: Number.MIN_VALUE }), 1e308, true);
  await roundtrip(t.Number({ multipleOf: 1e308 }), Number.MIN_VALUE, false);
  await roundtrip(t.Number({ multipleOf: 0.1 }), 0, true);
});

test("scalar schema options reject invalid divisors and Unicode patterns at definition", () => {
  for (const schema of [
    t.Number({ multipleOf: 0 }),
    t.Integer({ multipleOf: -1 }),
    t.Number({ multipleOf: Infinity }),
    t.String({ pattern: "{" }),
  ]) {
    expect(() =>
      defineService("invalid", {
        get: { method: "GET", path: "/", responses: { 200: schema } },
      }),
    ).toThrow();
  }
});

test("numbers and integers reject non-finite bodies and query values before dispatch", async () => {
  for (const schema of [t.Number(), t.Integer()]) {
    for (const value of [NaN, Infinity, -Infinity]) await roundtrip(schema, value, false);
    let calls = 0;
    const service = defineService("finite", {
      post: { method: "POST", path: "/", body: schema, responses: { 204: null } },
      get: { method: "GET", path: "/", query: t.Object({ n: schema }), responses: { 204: null } },
      read: { method: "GET", path: "/read", responses: { 200: schema } },
    });
    const handle = createServiceHandler(service, {
      post: () => {
        calls++;
        return { status: 204, body: undefined };
      },
      get: () => {
        calls++;
        return { status: 204, body: undefined };
      },
      read: () => ({ status: 200, body: 1 }),
    });
    for (const wire of ["1e400", "-1e400"]) {
      expect(
        (
          await handle(
            new Request("http://fixture/", {
              method: "POST",
              headers: { "content-type": "application/json" },
              body: wire,
            }),
          )
        ).status,
      ).toBe(400);
      const reader = createServiceClient(service, {
        baseURL: "http://fixture",
        fetch: async () => new Response(wire, { headers: { "content-type": "application/json" } }),
      });
      await expect(reader.read()).rejects.toMatchObject({ code: "validation", phase: "response" });
    }
    for (const wire of ["NaN", "Infinity", "-Infinity", "1e400", "-1e400"])
      expect((await handle(new Request(`http://fixture/?n=${wire}`))).status).toBe(400);
    const client = createServiceClient(service, {
      baseURL: "http://fixture",
      fetch: async () => {
        calls++;
        return new Response(null, { status: 204 });
      },
    });
    for (const n of [NaN, Infinity, -Infinity])
      await expect(client.get({ query: { n } })).rejects.toMatchObject({
        code: "validation",
        phase: "request",
      });
    expect(calls).toBe(0);
  }
});

test("interpreted scalar bounds and Unicode patterns compose without coercion", async () => {
  const number = t.Number({ minimum: -2, maximum: 2, exclusiveMinimum: -1, exclusiveMaximum: 1 });
  for (const value of [-2, -1, 1, 2, "0"]) await roundtrip(number, value, false);
  await roundtrip(number, 0, true);
  await roundtrip(t.Integer({ multipleOf: 2 }), 4, true);
  await roundtrip(t.Integer({ multipleOf: 2 }), 3, false);
  await roundtrip(t.Integer(), 0.5, false);
  await roundtrip(t.String({ pattern: "^.$" }), "😀", true);
  await roundtrip(t.String({ pattern: "^.$" }), "ab", false);
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
