import { describe, expect, test } from "bun:test";
import { createStorageBag } from "../src/storage";

const binding = {
  provider: "r2",
  bucket: "app-uploads",
  endpoint: "https://abc.r2.cloudflarestorage.com",
  region: "auto",
  access_key_id: "test-key",
  secret_access_key: "test-secret",
  public_base_url: "https://cdn.example.com/uploads",
} as const;

const fixedClock = () => new Date("2026-05-13T12:34:56.000Z");

describe("storage URLs", () => {
  test("creates deterministic signed download URLs", async () => {
    const storages = createStorageBag({ uploads: binding }, { now: fixedClock });

    const url = await storages.uploads?.createDownloadUrl("receipts/r 123.png", {
      expiresInSeconds: 3600,
      responseContentType: "image/png",
    });

    expect(
      url?.startsWith("https://app-uploads.abc.r2.cloudflarestorage.com/receipts/r%20123.png?"),
    ).toBe(true);
    const parsed = new URL(url ?? "");
    expect(parsed.searchParams.get("X-Amz-Algorithm")).toBe("AWS4-HMAC-SHA256");
    expect(parsed.searchParams.get("X-Amz-Credential")).toBe(
      "test-key/20260513/auto/s3/aws4_request",
    );
    expect(parsed.searchParams.get("X-Amz-Date")).toBe("20260513T123456Z");
    expect(parsed.searchParams.get("X-Amz-Expires")).toBe("3600");
    expect(parsed.searchParams.get("response-content-type")).toBe("image/png");
    expect(parsed.searchParams.get("X-Amz-Signature")).toMatch(/^[0-9a-f]{64}$/);
  });

  test("creates signed upload URLs with content-type in signed headers", async () => {
    const storages = createStorageBag({ uploads: binding }, { now: fixedClock });

    const url = await storages.uploads?.createUploadUrl("avatars/u_123.png", {
      contentType: "image/png",
    });

    const parsed = new URL(url ?? "");
    expect(parsed.searchParams.get("X-Amz-SignedHeaders")).toBe("content-type;host");
  });

  test("creates public optimized image URLs when public_base_url is requested", async () => {
    const storages = createStorageBag({ uploads: binding }, { now: fixedClock });

    const url = await storages.uploads?.createImageUrl("avatars/u_123.png", {
      public: true,
      width: 640,
      format: "webp",
    });

    expect(url).toBe(
      "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=640&f=webp",
    );
  });

  test("rejects transform options for private direct image URLs", async () => {
    const storages = createStorageBag({ uploads: { ...binding, public_base_url: undefined } });

    await expect(
      storages.uploads?.createImageUrl("avatars/u_123.png", { width: 640 }),
    ).rejects.toThrow("private storage image transforms");
  });
});
