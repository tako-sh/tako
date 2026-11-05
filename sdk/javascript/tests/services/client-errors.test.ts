import { expect, test } from "bun:test";
import { Type as t } from "@sinclair/typebox";
import { createServiceClient } from "../../src/services/client";
import { defineService, type ServiceInput } from "../../src/services/contract";

const service = defineService("errors", {
  get: {
    method: "GET",
    path: "/items/{id}",
    params: t.Object({ id: t.String() }),
    query: t.Object({ search: t.Optional(t.String()), tags: t.Optional(t.Array(t.String())) }),
    responses: { 200: t.String() },
  },
});

test("unexpected statuses retain ServiceError details when response cancellation fails", async () => {
  const cause = new Error("connection reset");
  for (const stream of [
    new ReadableStream({
      start(controller) {
        controller.error(cause);
      },
    }),
    new ReadableStream({
      cancel() {
        return Promise.reject(cause);
      },
    }),
  ]) {
    const client = createServiceClient(service, {
      baseURL: "http://fixture",
      fetch: async () => new Response(stream, { status: 503 }),
    });
    await expect(client.get({ params: { id: "ok" } })).rejects.toMatchObject({
      name: "ServiceError",
      code: "unexpected-status",
      operation: "get",
      phase: "response",
      status: 503,
      cause,
    });
  }
});

test("ill-formed UTF-16 URL values reject with field-specific validation errors before fetch", async () => {
  let calls = 0;
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async () => {
      calls++;
      return Response.json("ok");
    },
  });
  for (const text of ["\ud800", "\udc00", "a\ud800b"]) {
    const cases: [ServiceInput<typeof service.operations.get>, string][] = [
      [{ params: { id: text } }, "/params/id"],
      [{ params: { id: "ok" }, query: { search: text } }, "/query/search"],
      [{ params: { id: "ok" }, query: { tags: ["ok", text] } }, "/query/tags/1"],
    ];
    for (const [input, path] of cases) {
      await expect(client.get(input)).rejects.toMatchObject({
        name: "ServiceError",
        code: "validation",
        operation: "get",
        phase: "request",
        issues: [{ path, message: expect.any(String) }],
      });
    }
  }
  expect(calls).toBe(0);
});

test("valid Unicode URL values retain identity and ill-formed query names reject", async () => {
  const client = createServiceClient(service, {
    baseURL: "http://fixture",
    fetch: async (input) => {
      const url = new URL(input instanceof Request ? input.url : input);
      expect(decodeURIComponent(url.pathname.slice("/items/".length))).toBe("😀/雪");
      expect(url.searchParams.get("search")).toBe("😀/雪");
      return Response.json("ok");
    },
  });
  expect((await client.get({ params: { id: "😀/雪" }, query: { search: "😀/雪" } })).body).toBe(
    "ok",
  );
  const invalidKey = defineService("keys", {
    get: {
      method: "GET",
      path: "/",
      query: t.Object({ ["\ud800"]: t.String() }),
      responses: { 200: t.String() },
    },
  });
  let calls = 0;
  const invalidClient = createServiceClient(invalidKey, {
    baseURL: "http://fixture",
    fetch: async () => {
      calls++;
      return Response.json("ok");
    },
  });
  await expect(invalidClient.get({ query: { ["\ud800"]: "value" } })).rejects.toMatchObject({
    code: "validation",
    phase: "request",
  });
  expect(calls).toBe(0);
});
