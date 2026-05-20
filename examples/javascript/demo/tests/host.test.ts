import { describe, expect, test } from "bun:test";

import { baseHref, parseHost } from "../src/lib/host";

describe("demo host routing", () => {
  test("uses wildcard subdomains for production base pages", () => {
    const parsed = parseHost("valles-hub.demo.tako.sh");

    expect(parsed).toEqual({
      baseSlug: "valles-hub",
      rootHost: "demo.tako.sh",
      rootOrigin: "//demo.tako.sh",
    });
  });

  test("preserves the public port when deriving wildcard base links", () => {
    const parsed = parseHost("demo.test:47831");

    expect(baseHref(parsed, "europa-dock")).toBe("//europa-dock.demo.test:47831/");
  });

  test("uses localhost subdomains for local base links", () => {
    const parsed = parseHost("localhost:5173");

    expect(parsed).toEqual({
      rootHost: "localhost",
      rootOrigin: "//localhost:5173",
    });
    expect(baseHref(parsed, "titan-yard")).toBe("//titan-yard.localhost:5173/");
  });
});
