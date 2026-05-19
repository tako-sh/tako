import { describe, expect, test } from "bun:test";

import { baseHref, parseHost } from "../src/lib/host";

describe("demo host routing", () => {
  test("uses wildcard subdomains for production base pages", () => {
    const parsed = parseHost("valles-hub.demo.tako.sh");

    expect(parsed).toEqual({
      baseSlug: "valles-hub",
      routeStyle: "subdomain",
      rootHost: "demo.tako.sh",
      rootOrigin: "//demo.tako.sh",
    });
  });

  test("preserves the public port when deriving wildcard base links", () => {
    const parsed = parseHost("demo.test:47831");

    expect(parsed.routeStyle).toBe("subdomain");
    expect(baseHref(parsed, "europa-dock")).toBe("//europa-dock.demo.test:47831/");
  });

  test("falls back to path routes for unmanaged hosts", () => {
    const parsed = parseHost("localhost:5173");

    expect(parsed).toEqual({
      routeStyle: "path",
      rootHost: "localhost",
      rootOrigin: "//localhost:5173",
    });
    expect(baseHref(parsed, "titan-yard")).toBe("/bases/titan-yard");
  });
});
