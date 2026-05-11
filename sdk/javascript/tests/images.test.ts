import { Buffer } from "node:buffer";
import { afterEach, beforeEach, describe, expect, setSystemTime, test } from "bun:test";
import { createImageUrl } from "../src/images";
import { injectBootstrap } from "../src/tako/secrets";

describe("createImageUrl", () => {
  beforeEach(() => {
    setSystemTime();
    injectBootstrap({
      token: "internal-token",
      imageSecret: "image-secret",
      secrets: {},
    });
  });

  afterEach(() => {
    setSystemTime();
  });

  test("creates path-based private AVIF image URLs by default", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const url = createImageUrl("/assets/avatar.png", {
      width: 640,
    });

    expect(url).toStartWith("/_tako/image/v1/");
    expect(url).not.toContain("?");
    expect(decodePayload(url)).toEqual({
      w: 640,
      e: 1767830400,
      s: "/assets/avatar.png",
    });
  });

  test("creates optimized image URLs with omitted options", () => {
    const url = createImageUrl("/assets/photo.jpg");
    const payload = decodePayload(url);

    expect(payload).not.toHaveProperty("w");
    expect(payload).toMatchObject({
      s: "/assets/photo.jpg",
    });
    expect(typeof payload.e).toBe("number");
  });

  test("omits default width from heightless payloads", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const url = createImageUrl("/assets/photo.jpg", {
      width: 1200,
    });

    expect(decodePayload(url)).toEqual({
      e: 1767830400,
      s: "/assets/photo.jpg",
    });
  });

  test("creates explicit WebP fallback image URLs", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const url = createImageUrl("/assets/avatar.png", {
      width: 640,
      format: "webp",
    });

    expect(decodePayload(url)).toEqual({
      f: "webp",
      w: 640,
      e: 1767830400,
      s: "/assets/avatar.png",
    });
  });

  test("adds private browser cache max-age only when it overrides the default", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const url = createImageUrl("/assets/avatar.png", {
      width: 640,
      expiresInSeconds: 86_400,
      browserCacheMaxAgeSeconds: 3_600,
    });

    expect(decodePayload(url)).toEqual({
      w: 640,
      c: 3_600,
      e: 1767312000,
      s: "/assets/avatar.png",
    });
  });

  test("creates cover crop image URLs when height is set", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const url = createImageUrl("/assets/avatar.png", {
      width: 640,
      height: 640,
      crop: "smart",
    });

    expect(decodePayload(url)).toEqual({
      w: 640,
      h: 640,
      crop: "smart",
      e: 1767830400,
      s: "/assets/avatar.png",
    });
  });

  test("creates contain image URLs without crop", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const url = createImageUrl("/assets/hero.jpg", {
      width: 640,
      height: 384,
      fit: "contain",
    });

    expect(decodePayload(url)).toEqual({
      w: 640,
      h: 384,
      fit: "contain",
      e: 1767830400,
      s: "/assets/hero.jpg",
    });
  });

  test("creates stable public image URLs without an expiration", () => {
    setSystemTime(new Date("2026-01-01T00:00:00Z"));

    const first = createImageUrl("/assets/hero.jpg", {
      width: 1200,
      quality: 80,
      public: true,
    });
    setSystemTime(new Date("2026-01-02T00:00:00Z"));
    const second = createImageUrl("/assets/hero.jpg", {
      width: 1200,
      quality: 80,
      public: true,
    });

    expect(first).toBe(second);
    expect(decodePayload(first)).toEqual({
      pub: true,
      q: 80,
      s: "/assets/hero.jpg",
    });
  });

  test("rejects unsupported output formats before signing", () => {
    expect(() =>
      createImageUrl("/assets/avatar.png", { width: 640, format: "avif" as never }),
    ).toThrow(/omit image format/);
    expect(() =>
      createImageUrl("/assets/avatar.png", { width: 640, format: "png" as never }),
    ).toThrow(/unsupported image format/);
  });

  test("rejects browser cache options on public URLs", () => {
    expect(() =>
      createImageUrl("/assets/hero.jpg", {
        width: 1200,
        public: true,
        browserCacheMaxAgeSeconds: 3_600,
      } as never),
    ).toThrow(/public image URLs cannot set browser cache/);
  });

  test("rejects unsupported widths before signing", () => {
    expect(() => createImageUrl("/assets/avatar.png", { width: 641 })).toThrow(
      /unsupported image width/,
    );
  });

  test("rejects incompatible resize options before signing", () => {
    expect(() => createImageUrl("/assets/avatar.png", { width: 640, height: 641 })).toThrow(
      /unsupported image height/,
    );
    expect(() =>
      createImageUrl("/assets/avatar.png", { width: 640, fit: "cover" } as never),
    ).toThrow(/fit and crop require height/);
    expect(() =>
      createImageUrl("/assets/avatar.png", {
        height: 640,
      } as never),
    ).toThrow(/height requires width/);
    expect(() =>
      createImageUrl("/assets/avatar.png", {
        width: 640,
        height: 640,
        fit: "contain",
        crop: "smart",
      } as never),
    ).toThrow(/crop requires fit/);
    expect(() =>
      createImageUrl("/assets/avatar.png", {
        width: 640,
        height: 640,
        fit: "inside",
      } as never),
    ).toThrow(/unsupported image fit/);
    expect(() =>
      createImageUrl("/assets/avatar.png", {
        width: 640,
        height: 640,
        crop: "entropy",
      } as never),
    ).toThrow(/unsupported image crop/);
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

function decodePayload(url: string): Record<string, unknown> {
  const token = url.slice("/_tako/image/v1/".length);
  expect(token).not.toContain("/");
  const [payload, signature, extra] = token.split(".");
  expect(payload).toBeTruthy();
  expect(signature).toBeTruthy();
  expect(extra).toBeUndefined();
  return JSON.parse(Buffer.from(payload!, "base64url").toString("utf8"));
}
