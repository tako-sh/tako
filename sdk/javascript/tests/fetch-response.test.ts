import { describe, expect, test } from "bun:test";

import { normalizeFetchResponse } from "../src/tako/fetch-response";

describe("normalizeFetchResponse", () => {
  test("returns native Response objects unchanged", () => {
    const response = new Response("ok", { status: 201 });

    expect(normalizeFetchResponse(response)).toBe(response);
  });

  test("unwraps Response-compatible framework shims", () => {
    const native = new Response("ok", {
      status: 202,
      headers: { "x-test": "yes" },
    });
    const shim = Object.create(Response.prototype, {
      _response: {
        get: () => native,
      },
    }) as Response;

    expect(shim instanceof Response).toBe(true);
    expect(normalizeFetchResponse(shim)).toBe(native);
  });
});
