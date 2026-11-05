import { expect, test } from "bun:test";
import { Type as t } from "@sinclair/typebox";
import { createServiceClient } from "../../src/services/client";
import { defineService } from "../../src/services/contract";

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
