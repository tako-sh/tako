import { expect, test } from "bun:test";
import { Type as t } from "@sinclair/typebox";
import { defineService } from "../../src/services/contract";
import { serviceManifest, serviceOpenApi, serializeServiceDocument } from "../../src/services/manifest";

const get = { method: "GET", path: "/items/{id}", params: t.Object({ id: t.String() }), responses: { 200: t.Object({ title: t.String() }), 404: t.Object({ error: t.String() }) } } as const;

test("exports deterministic JSON schemas and standard OpenAPI", () => {
  const service = defineService("items", { get });
  expect(JSON.parse(JSON.stringify(serviceManifest(service)))).toEqual({ name: "items", operations: JSON.parse(JSON.stringify({ get })) });
  const document = serviceOpenApi([service]);
  expect(document).toMatchObject({ openapi: "3.1.0", paths: { "/items/{id}": { get: { operationId: "items.get", parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" }, style: "simple" }] } } } });
  expect(serializeServiceDocument({ b: 1, a: { y: 1, x: 2 } })).toBe(serializeServiceDocument({ a: { x: 2, y: 1 }, b: 1 }));
});

test("rejects contracts that cannot represent ordinary HTTP", () => {
  expect(() => defineService("items", { get: { ...get, params: t.Object({ other: t.String() }) } })).toThrow("placeholders");
  expect(() => defineService("items", { get, duplicate: { ...get, path: "/items/{other}", params: t.Object({ other: t.String() }) } })).toThrow("Duplicate");
  expect(() => defineService("items", { get: { ...get, body: t.String() } })).toThrow("request body");
  expect(() => defineService("items", { get: { ...get, responses: { 204: t.String() } } })).toThrow("empty response");
  expect(() => defineService("items", { get: { ...get, responses: { 200: t.Date() } } })).toThrow("Unsupported");
  expect(() => defineService("items", { get: { ...get, query: t.Object({ nested: t.Object({}) }) } })).toThrow("scalars");
  expect(() => serviceOpenApi([defineService("a", { get }), defineService("b", { get })])).toThrow("Duplicate HTTP route");
});
