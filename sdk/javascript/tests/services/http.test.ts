import { expect, test } from "bun:test";
import { Type as t } from "@sinclair/typebox";
import { defineService } from "../../src/services/contract";
import { createServiceClient } from "../../src/services/client";
import { createServiceHandler } from "../../src/services/handler";
import { ServiceError } from "../../src/services/http";

const service = defineService("catalog", {
  get: {
    method: "GET",
    path: "/items/{id}",
    params: t.Object({ id: t.String() }),
    query: t.Object(
      {
        tags: t.Optional(t.Array(t.String())),
        limit: t.Optional(t.Integer()),
        active: t.Optional(t.Boolean()),
      },
      { additionalProperties: false },
    ),
    responses: {
      200: t.Object({
        id: t.String(),
        tags: t.Array(t.String()),
        limit: t.Number(),
        active: t.Boolean(),
      }),
      404: t.Object({ error: t.String() }),
    },
  },
  create: {
    method: "POST",
    path: "/items",
    body: t.Object({ title: t.String({ minLength: 1 }) }),
    responses: { 201: t.Object({ title: t.String() }) },
  },
  remove: { method: "DELETE", path: "/items", responses: { 204: null } },
  optional: {
    method: "POST",
    path: "/optional",
    body: t.Optional(t.String()),
    responses: { 200: t.String() },
  },
});
const handler = createServiceHandler(
  service,
  {
    get: ({ params, query }) =>
      params.id === "missing"
        ? { status: 404, body: { error: "missing" } }
        : {
            status: 200,
            body: {
              id: params.id,
              tags: query?.tags ?? [],
              limit: query?.limit ?? 0,
              active: query?.active ?? false,
            },
          },
    create: ({ body }, request) => ({
      status: 201,
      body,
      headers: { "x-auth": request.headers.get("authorization") ?? "none" },
    }),
    remove: () => ({ status: 204, body: undefined }),
    optional: ({ body }) => ({ status: 200, body: body ?? "absent" }),
  },
  { basePath: "/api" },
);

test("real fetch roundtrips params, query scalars and arrays, JSON, statuses, and empty responses", async () => {
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: handler });
  try {
    const client = createServiceClient(service, {
      baseURL: `${server.url}api/`,
      headers: { authorization: "Bearer fixture" },
    });
    const result = await client.get({
      params: { id: "slash/space ?%雪" },
      query: { tags: ["a&b", "c/d"], limit: 12, active: true },
    });
    expect(result).toMatchObject({
      status: 200,
      body: { id: "slash/space ?%雪", tags: ["a&b", "c/d"], limit: 12, active: true },
    });
    expect(await client.get({ params: { id: "missing" } })).toMatchObject({
      status: 404,
      body: { error: "missing" },
    });
    const created = await client.create({ body: { title: "hello" } });
    expect(created.body).toEqual({ title: "hello" });
    expect(created.headers.get("x-auth")).toBe("Bearer fixture");
    expect(await client.remove()).toMatchObject({ status: 204, body: undefined });
    expect(await client.optional()).toMatchObject({ body: "absent" });
    expect(await client.optional({ body: "present" })).toMatchObject({ body: "present" });
  } finally {
    await server.stop(true);
  }
});

test("ordinary server needs no SDK adapter or wire envelope", async () => {
  const ordinary = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch: (request) => {
      expect(request.method).toBe("POST");
      expect(new URL(request.url).pathname).toBe("/items");
      return request.json().then((body) => Response.json(body, { status: 201 }));
    },
  });
  try {
    const client = createServiceClient(service, { baseURL: ordinary.url });
    expect(await client.create({ body: { title: "ordinary" } })).toMatchObject({
      status: 201,
      body: { title: "ordinary" },
    });
  } finally {
    await ordinary.stop(true);
  }
});

test("validates caller input before fetch and returns useful errors", async () => {
  let calls = 0;
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async () => {
      calls++;
      return new Response();
    },
  });
  await expect(client.create({ body: { title: "" } })).rejects.toMatchObject({
    name: "ServiceError",
    code: "validation",
    phase: "request",
    operation: "create",
    issues: [{ path: "/body/title", message: expect.any(String) }],
  });
  await expect(client.get({ params: { id: ".." } })).rejects.toBeInstanceOf(ServiceError);
  await expect(client.get({ params: { id: "ok" }, query: { tags: [] } })).rejects.toThrow(
    "must not be empty",
  );
  expect(calls).toBe(0);
});

test("rejects unexpected status, wrong content type, malformed or invalid JSON, and nonempty empty responses", async () => {
  for (const [response, code] of [
    [new Response("unavailable", { status: 503 }), "unexpected-status"],
    [new Response('{"title":"hi"}', { status: 201 }), "content-type"],
    [
      new Response("{", { status: 201, headers: { "content-type": "application/json" } }),
      "invalid-json",
    ],
    [Response.json({ title: 4 }, { status: 201 }), "validation"],
  ] as const) {
    const client = createServiceClient(service, {
      baseURL: "http://fixture",
      fetch: async () => response,
    });
    await expect(client.create({ body: { title: "hi" } })).rejects.toMatchObject({
      code,
      phase: "response",
      status: response.status,
    });
  }
  const empty = defineService("empty", {
    get: { method: "GET", path: "/", responses: { 200: null } },
  });
  await expect(
    createServiceClient(empty, {
      baseURL: "http://fixture",
      fetch: async () => new Response("unexpected"),
    }).get(),
  ).rejects.toThrow("empty response");
  const cause = new Error("offline");
  await expect(
    createServiceClient(empty, {
      baseURL: "http://fixture",
      fetch: async () => {
        throw cause;
      },
    }).get(),
  ).rejects.toMatchObject({ code: "transport", cause });
});

test("handler validates standard requests and preserves HTTP routing", async () => {
  for (const request of [
    new Request("http://fixture/api/items", {
      method: "POST",
      body: '{"title":4}',
      headers: { "content-type": "application/json" },
    }),
    new Request("http://fixture/api/items", {
      method: "POST",
      body: "{",
      headers: { "content-type": "application/json" },
    }),
    new Request("http://fixture/api/items/%ZZ"),
    new Request("http://fixture/api/items/a?limit=invalid"),
    new Request("http://fixture/api/items/a?limit=1&limit=2"),
    new Request("http://fixture/api/items/a?active=TRUE"),
    new Request("http://fixture/api/items/a?unknown=1"),
  ])
    expect((await handler(request)).status).toBe(400);
  const wrongMethod = await handler(new Request("http://fixture/api/items", { method: "PUT" }));
  expect(wrongMethod.status).toBe(405);
  expect(wrongMethod.headers.get("allow")).toBe("POST, DELETE");
  expect((await handler(new Request("http://fixture/other"))).status).toBe(404);
});

test("handler validates responses before sending them", async () => {
  const broken = defineService("broken", {
    get: { method: "GET", path: "/", responses: { 200: t.String() } },
  });
  const invalid = createServiceHandler(broken, {
    // @ts-expect-error Exercise a broken JavaScript handler at runtime.
    get: () => ({ status: 200, body: 42 }),
  });
  await expect(invalid(new Request("http://fixture/"))).rejects.toMatchObject({
    code: "validation",
    phase: "response",
    status: 200,
  });
});

test("standard Request/Response adapter roundtrip without a listener", async () => {
  const client = createServiceClient(service, {
    baseURL: "http://fixture/api",
    fetch: (input, init) => handler(new Request(input, init)),
  });
  expect(
    await client.get({
      params: { id: "a/b ?%雪" },
      query: { tags: ["a&b", "c/d"], active: false, limit: 0 },
    }),
  ).toMatchObject({
    status: 200,
    body: { id: "a/b ?%雪", tags: ["a&b", "c/d"], active: false, limit: 0 },
  });
  expect(await client.get({ params: { id: "missing" } })).toMatchObject({
    status: 404,
    body: { error: "missing" },
  });
  expect(await client.create({ body: { title: "works" } })).toMatchObject({
    status: 201,
    body: { title: "works" },
  });
  expect(await client.remove()).toMatchObject({ status: 204, body: undefined });
  expect(await client.optional()).toMatchObject({ body: "absent" });
});

test("schema constraints and undeclared query parameters remain enforced", async () => {
  const constrained = defineService("constrained", {
    get: {
      method: "GET",
      path: "/",
      query: t.Object({ count: t.Integer({ minimum: 1, maximum: 3 }) }),
      responses: { 200: t.Array(t.String(), { minItems: 1 }) },
    },
  });
  const handle = createServiceHandler(constrained, { get: () => ({ status: 200, body: ["ok"] }) });
  expect((await handle(new Request("http://fixture/?count=2&unknown=value"))).status).toBe(400);
  expect((await handle(new Request("http://fixture/?count=0"))).status).toBe(400);
  expect((await handle(new Request("http://fixture/?count=2"))).status).toBe(200);
});

test("aborted body streams produce typed transport errors", async () => {
  const cause = new Error("stream interrupted");
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async () =>
      new Response(
        new ReadableStream({
          start(controller) {
            controller.error(cause);
          },
        }),
        { status: 201, headers: { "content-type": "application/json" } },
      ),
  });
  await expect(client.create({ body: { title: "ok" } })).rejects.toMatchObject({
    code: "transport",
    phase: "response",
    cause,
  });
});

test("quoted status keys retain numeric response types", async () => {
  const quoted = defineService("quoted", {
    get: { method: "GET", path: "/", responses: { "200": t.String() } },
  });
  const handle = createServiceHandler(quoted, { get: () => ({ status: 200, body: "ok" }) });
  const response = await createServiceClient(quoted, {
    baseURL: "http://fixture",
    fetch: (input, init) => handle(new Request(input, init)),
  }).get();
  const status: 200 = response.status;
  const body: string = response.body;
  expect({ status, body }).toEqual({ status: 200, body: "ok" });
});
