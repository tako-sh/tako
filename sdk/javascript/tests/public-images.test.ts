import { describe, expect, test } from "bun:test";
import { imageSrcSet, imageUrl } from "../src";

describe("imageUrl", () => {
  test("builds canonical public local image optimizer URLs", () => {
    expect(imageUrl("/assets/hero.jpg", { width: 1200 })).toBe(
      "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1200",
    );
  });

  test("builds canonical public remote image optimizer URLs", () => {
    expect(
      imageUrl("https://cdn.example.com/uploads/avatar.jpg?v=1", {
        width: 640,
        quality: 80,
        format: "webp",
      }),
    ).toBe(
      "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg%3Fv%3D1&w=640&q=80&f=webp",
    );
  });

  test("defaults width and quality without creating an extra quality variant", () => {
    expect(imageUrl("/assets/photo.jpg")).toBe("/_tako/image?src=%2Fassets%2Fphoto.jpg&w=1200");
  });

  test("rejects malformed public sources", () => {
    for (const source of [
      "",
      "//cdn.example.com/avatar.jpg",
      "/_tako/image?src=%2Favatar.jpg&w=640",
      "data:image/png;base64,abc",
      "https://user@example.com/avatar.jpg",
      "https://example.com/avatar.jpg#fragment",
    ]) {
      expect(() => imageUrl(source, { width: 640 })).toThrow(/invalid image source/);
    }
  });

  test("rejects unsupported options before building the URL", () => {
    expect(() => imageUrl("/avatar.jpg", { width: 641 })).toThrow(/unsupported image width/);
    expect(() => imageUrl("/avatar.jpg", { quality: 0 })).toThrow(
      /image quality must be an integer/,
    );
    expect(() => imageUrl("/avatar.jpg", { format: "jpeg" as never })).toThrow(
      /unsupported image format/,
    );
  });
});

describe("imageSrcSet", () => {
  test("builds constrained responsive image sources", () => {
    expect(
      imageSrcSet("/assets/hero.jpg", {
        layout: "constrained",
        width: 1200,
        quality: 80,
      }),
    ).toEqual({
      src: "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1200&q=80",
      srcSet: [
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=320&q=80 320w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=640&q=80 640w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=960&q=80 960w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1200&q=80 1200w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1920&q=80 1920w",
      ].join(", "),
      sizes: "(min-width: 1200px) 1200px, 100vw",
    });
  });

  test("builds full-width responsive image sources", () => {
    expect(
      imageSrcSet("/assets/hero.jpg", {
        layout: "full-width",
        width: 1920,
      }),
    ).toEqual({
      src: "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1920",
      srcSet: [
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=320 320w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=640 640w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=960 960w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1200 1200w",
        "/_tako/image?src=%2Fassets%2Fhero.jpg&w=1920 1920w",
      ].join(", "),
      sizes: "100vw",
    });
  });

  test("allows explicit widths and sizes", () => {
    expect(
      imageSrcSet("https://cdn.example.com/uploads/avatar.jpg", {
        width: 960,
        widths: [320, 640],
        sizes: "(max-width: 768px) 100vw, 50vw",
        format: "webp",
      }),
    ).toEqual({
      src: "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg&w=960&f=webp",
      srcSet: [
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg&w=320&f=webp 320w",
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg&w=640&f=webp 640w",
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatar.jpg&w=960&f=webp 960w",
      ].join(", "),
      sizes: "(max-width: 768px) 100vw, 50vw",
    });
  });

  test("rejects invalid responsive image options", () => {
    expect(() => imageSrcSet("/avatar.jpg", { width: 641 })).toThrow(/unsupported image width/);
    expect(() => imageSrcSet("/avatar.jpg", { width: 640, widths: [] })).toThrow(
      /image widths must include at least one width/,
    );
    expect(() => imageSrcSet("/avatar.jpg", { width: 640, layout: "cover" as never })).toThrow(
      /unsupported image layout/,
    );
    expect(() => imageSrcSet("/avatar.jpg", { width: 640, sizes: "   " })).toThrow(
      /image sizes must be a non-empty string/,
    );
  });
});
