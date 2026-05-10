import { beforeEach, describe, expect, test } from "bun:test";
import { createImageUrl } from "../src/images";
import { injectBootstrap } from "../src/tako/secrets";

describe("createImageUrl", () => {
  beforeEach(() => {
    injectBootstrap({
      token: "internal-token",
      imageSecret: "image-secret",
      secrets: {},
    });
  });

  test("creates path-based private image URLs by default", () => {
    const url = createImageUrl("/assets/avatar.png", {
      width: 640,
      now: new Date("2026-01-01T00:00:00Z"),
    });

    expect(url).toStartWith("/_tako/image/v1/private/640/75/1767312000/");
    expect(url).not.toContain("?");
  });

  test("creates stable public image URLs without an expiration", () => {
    const first = createImageUrl("/assets/hero.jpg", {
      width: 1200,
      quality: 80,
      public: true,
      now: new Date("2026-01-01T00:00:00Z"),
    });
    const second = createImageUrl("/assets/hero.jpg", {
      width: 1200,
      quality: 80,
      public: true,
      now: new Date("2026-01-02T00:00:00Z"),
    });

    expect(first).toBe(second);
    expect(first).toStartWith("/_tako/image/v1/public/1200/80/-/");
  });

  test("rejects unsupported widths before signing", () => {
    expect(() => createImageUrl("/assets/avatar.png", { width: 641 })).toThrow(
      /unsupported image width/,
    );
  });

  test("rejects private and local remote hosts before signing", () => {
    for (const source of [
      "http://127.0.0.1/avatar.png",
      "http://[::1]/avatar.png",
      "http://localhost/avatar.png",
      "http://assets.localhost/avatar.png",
    ]) {
      expect(() => createImageUrl(source, { width: 640 })).toThrow(/invalid image source/);
    }
  });

  test("requires the Tako image secret", () => {
    injectBootstrap({ token: "internal-token", imageSecret: null, secrets: {} });

    expect(() => createImageUrl("/assets/avatar.png", { width: 640 })).toThrow(
      /image service is not available/,
    );
  });
});
