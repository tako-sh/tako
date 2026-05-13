import { describe, expect, test } from "bun:test";
import { imageUrl } from "../src";

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
