import { expect, test } from "bun:test";
import { Type as t } from "@sinclair/typebox";
import { defineService } from "../../src/services/contract";
import { generateServiceDocument } from "../../src/services/generate";
import {
  serializeServiceDocument,
  serviceManifest,
  serviceOpenApi,
} from "../../src/services/manifest";

test("serialized manifests preserve required and optional request body presence", () => {
  const required = defineService("body", {
    post: { method: "POST", path: "/", body: t.String(), responses: { 204: null } },
  });
  const optional = defineService("body", {
    post: { ...required.operations.post, body: t.Optional(t.String()) },
  });
  const absent = defineService("body", {
    get: { method: "GET", path: "/", responses: { 204: null } },
  });
  expect(serviceManifest(required)).toMatchObject({
    operations: { post: { bodyRequired: true, body: { type: "string" } } },
  });
  expect(serviceManifest(optional)).toMatchObject({
    operations: { post: { bodyRequired: false, body: { type: "string" } } },
  });
  expect(serviceManifest(absent)).toEqual({
    name: "body",
    operations: { get: { method: "GET", path: "/", responses: { "204": null } } },
  });
  expect(serializeServiceDocument(serviceManifest(required))).not.toBe(
    serializeServiceDocument(serviceManifest(optional)),
  );
  for (const [service, bodyRequired] of [
    [required, true],
    [optional, false],
  ] as const) {
    expect(JSON.parse(generateServiceDocument([service], "manifest"))).toMatchObject({
      services: [{ operations: { post: { bodyRequired } } }],
    });
    expect(serviceOpenApi([service])).toMatchObject({
      paths: { "/": { post: { requestBody: { required: bodyRequired } } } },
    });
  }
});
