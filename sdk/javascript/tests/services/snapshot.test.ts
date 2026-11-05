import { expect, test } from "bun:test";
import { Type as t, type TSchema } from "@sinclair/typebox";
import { createServiceClient, createServiceHandler, defineService } from "../../src/services";

const roleSchema = t.Object({ role: t.Literal("user") }, { additionalProperties: false });

async function outgoing(
  schema: TSchema,
  make: () => { value: unknown; verify: () => void },
  expected: string,
) {
  const service = defineService("snapshot", {
    post: { method: "POST", path: "/", body: schema, responses: { 204: null } },
    get: { method: "GET", path: "/", responses: { 200: schema } },
  });
  const sent = make();
  let inputReads = 0;
  let fetchCalls = 0;
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async (_input, init) => {
      fetchCalls++;
      expect(init?.body).toBe(expected);
      return new Response(null, { status: 204 });
    },
  });
  await client.post({
    get body() {
      inputReads++;
      return sent.value;
    },
  });
  expect(fetchCalls).toBe(1);
  expect(inputReads).toBe(1);
  sent.verify();
  const returned = make();
  let outputReads = 0;
  const handler = createServiceHandler(service, {
    post: () => ({ status: 204, body: undefined }),
    get: () => ({
      status: 200,
      get body() {
        outputReads++;
        return returned.value;
      },
    }),
  });
  expect(await (await handler(new Request("http://fixture/"))).text()).toBe(expected);
  expect(outputReads).toBe(1);
  returned.verify();
}

test("outgoing snapshots ignore inherited toJSON without touching the caller", async () => {
  await outgoing(
    roleSchema,
    () => {
      let calls = 0;
      const prototype = {
        toJSON() {
          calls++;
          return { role: "admin" };
        },
      };
      const value = Object.assign(Object.create(prototype) as { role: string }, { role: "user" });
      return {
        value,
        verify() {
          expect(calls).toBe(0);
          expect(Object.getPrototypeOf(value)).toBe(prototype);
          expect(Object.hasOwn(value, "toJSON")).toBe(false);
        },
      };
    },
    '{"role":"user"}',
  );
});

test("changing getters and aliased nested objects are read once into the checked snapshot", async () => {
  await outgoing(
    t.Object({ records: t.Array(roleSchema) }),
    () => {
      let reads = 0;
      const shared = {
        get role() {
          return ++reads === 1 ? "user" : "admin";
        },
      };
      return {
        value: { records: [shared, shared] },
        verify() {
          expect(reads).toBe(1);
        },
      };
    },
    '{"records":[{"role":"user"},{"role":"user"}]}',
  );
});

test("input and result body getters cannot replace the value after validation", async () => {
  const service = defineService("body-getter", {
    post: { method: "POST", path: "/", body: roleSchema, responses: { 200: roleSchema } },
  });
  let inputReads = 0;
  let outputReads = 0;
  const handler = createServiceHandler(service, {
    post: () => ({
      status: 200,
      get body() {
        return { role: ++outputReads === 1 ? "user" : "admin" } as { role: "user" };
      },
    }),
  });
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: (input, init) => {
      expect(init?.body).toBe('{"role":"user"}');
      return handler(new Request(input, init));
    },
  });
  expect(
    (
      await client.post({
        get body() {
          return { role: ++inputReads === 1 ? "user" : "admin" } as { role: "user" };
        },
      })
    ).body,
  ).toEqual({ role: "user" });
  expect(inputReads).toBe(1);
  expect(outputReads).toBe(1);
});

test("URL encoding uses the same checked params and query snapshots", async () => {
  let paramReads = 0;
  let queryReads = 0;
  const service = defineService("url-snapshot", {
    get: {
      method: "GET",
      path: "/{id}",
      params: t.Object({ id: t.Literal("user") }),
      query: t.Object({ role: t.Literal("user") }),
      responses: { 204: null },
    },
  });
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async (input) => {
      expect(input instanceof URL && input.href).toBe("http://fixture/user?role=user");
      return new Response(null, { status: 204 });
    },
  });
  await client.get({
    params: {
      get id() {
        return (++paramReads === 1 ? "user" : "admin") as "user";
      },
    },
    query: {
      get role() {
        return (++queryReads === 1 ? "user" : "admin") as "user";
      },
    },
  });
  expect(paramReads).toBe(1);
  expect(queryReads).toBe(1);
});

test("array snapshots use own indices without custom iterators or inherited toJSON", async () => {
  await outgoing(
    t.Array(roleSchema),
    () => {
      let reads = 0;
      let calls = 0;
      const value: unknown[] = [];
      Object.defineProperty(value, "0", {
        enumerable: true,
        configurable: true,
        get() {
          reads++;
          return { role: reads === 1 ? "user" : "admin" };
        },
      });
      const prototype = Object.create(Array.prototype) as object;
      Object.defineProperty(prototype, "toJSON", {
        value: () => {
          calls++;
          return [{ role: "admin" }];
        },
      });
      Object.defineProperty(prototype, Symbol.iterator, {
        value: function* () {
          calls++;
          yield { role: "admin" };
        },
      });
      Object.setPrototypeOf(value, prototype);
      return {
        value,
        verify() {
          expect(reads).toBe(1);
          expect(calls).toBe(0);
          expect(Object.getPrototypeOf(value)).toBe(prototype);
        },
      };
    },
    '[{"role":"user"}]',
  );
});

test("unserializable own values, cycles, and failing getters cause typed errors before fetch", async () => {
  const service = defineService("json", {
    post: { method: "POST", path: "/", body: t.Object({}), responses: { 204: null } },
    get: { method: "GET", path: "/", responses: { 200: t.Object({}) } },
  });
  const cycle: Record<string, unknown> = {};
  cycle["self"] = cycle;
  const cause = new Error("getter failed");
  let calls = 0;
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async () => {
      calls++;
      return new Response(null, { status: 204 });
    },
  });
  for (const body of [
    ...[undefined, () => 1, 1n, Symbol("value"), NaN, Infinity, -Infinity].map((value) => ({
      value,
    })),
    cycle,
    {
      get role() {
        throw cause;
      },
    },
  ]) {
    await expect(client.post({ body })).rejects.toMatchObject({
      code: "validation",
      phase: "request",
    });
    const handler = createServiceHandler(service, {
      post: () => ({ status: 204, body: undefined }),
      get: () => ({ status: 200, body }),
    });
    await expect(handler(new Request("http://fixture/"))).rejects.toMatchObject({
      code: "validation",
      phase: "response",
    });
  }
  expect(calls).toBe(0);
});

test("overflow numbers in unconstrained additional JSON properties are also rejected", async () => {
  const service = defineService("json", {
    post: { method: "POST", path: "/", body: t.Object({}), responses: { 200: t.Object({}) } },
  });
  let calls = 0;
  const handler = createServiceHandler(service, {
    post: ({ body }) => {
      calls++;
      return { status: 200, body };
    },
  });
  const wire = '{"extra":[1e400]}';
  expect(
    (
      await handler(
        new Request("http://fixture/", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: wire,
        }),
      )
    ).status,
  ).toBe(400);
  expect(calls).toBe(0);
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async () => new Response(wire, { headers: { "content-type": "application/json" } }),
  });
  await expect(client.post({ body: {} })).rejects.toMatchObject({
    code: "validation",
    phase: "response",
  });
});
