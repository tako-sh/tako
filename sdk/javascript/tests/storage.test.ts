import { describe, expect, test } from "bun:test";
import { createStorageBag } from "../src/storage";

const binding = {
  provider: "s3",
  bucket: "app-uploads",
  endpoint: "https://abc.r2.cloudflarestorage.com",
  region: "auto",
  access_key_id: "test-key",
  secret_access_key: "test-secret",
  public_base_url: "https://cdn.example.com/uploads",
} as const;

const fixedClock = () => new Date("2026-05-13T12:34:56.000Z");

function requireUploads(storages: ReturnType<typeof createStorageBag>) {
  const uploads = storages.uploads;
  if (!uploads) throw new Error("missing uploads storage");
  return uploads;
}

describe("storage URLs", () => {
  test("creates deterministic signed download URLs", async () => {
    const storages = createStorageBag({ uploads: binding }, { now: fixedClock });
    const uploads = requireUploads(storages);

    const url = await uploads.createDownloadUrl("receipts/r 123.png", {
      expiresInSeconds: 3600,
      responseContentType: "image/png",
    });

    expect(
      url.startsWith("https://app-uploads.abc.r2.cloudflarestorage.com/receipts/r%20123.png?"),
    ).toBe(true);
    const parsed = new URL(url);
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
    const uploads = requireUploads(storages);

    const url = await uploads.createUploadUrl("avatars/u_123.png", {
      contentType: "image/png",
    });

    const parsed = new URL(url);
    expect(parsed.searchParams.get("X-Amz-SignedHeaders")).toBe("content-type;host");
  });

  test("creates public optimized image URLs when public_base_url is requested", async () => {
    const storages = createStorageBag({ uploads: binding }, { now: fixedClock });
    const uploads = requireUploads(storages);

    const url = await uploads.createImageUrl("avatars/u_123.png", {
      public: true,
      width: 640,
      format: "webp",
    });

    expect(url).toBe(
      "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=640&f=webp",
    );
  });

  test("creates public optimized image srcsets when public_base_url is requested", async () => {
    const storages = createStorageBag({ uploads: binding }, { now: fixedClock });
    const uploads = requireUploads(storages);

    const image = await uploads.createImageSrcSet("avatars/u_123.png", {
      public: true,
      layout: "constrained",
      width: 1200,
      format: "webp",
    });

    expect(image).toEqual({
      src: "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=1200&f=webp",
      srcSet: [
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=320&f=webp 320w",
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=640&f=webp 640w",
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=960&f=webp 960w",
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=1200&f=webp 1200w",
        "/_tako/image?src=https%3A%2F%2Fcdn.example.com%2Fuploads%2Favatars%2Fu_123.png&w=1920&f=webp 1920w",
      ].join(", "),
      sizes: "(min-width: 1200px) 1200px, 100vw",
    });
  });

  test("rejects transform options for private direct image URLs", async () => {
    const storages = createStorageBag({ uploads: { ...binding, public_base_url: undefined } });
    const uploads = requireUploads(storages);

    let error: unknown;
    try {
      await uploads.createImageUrl("avatars/u_123.png", { width: 640 });
    } catch (caught) {
      error = caught;
    }

    expect(error).toBeInstanceOf(TypeError);
    expect((error as Error).message).toContain("private storage image transforms");
  });

  test("rejects private storage image srcsets for now", async () => {
    const storages = createStorageBag({ uploads: { ...binding, public_base_url: undefined } });
    const uploads = requireUploads(storages);

    let error: unknown;
    try {
      await uploads.createImageSrcSet("avatars/u_123.png", {
        width: 1200,
      });
    } catch (caught) {
      error = caught;
    }

    expect(error).toBeInstanceOf(TypeError);
    expect((error as Error).message).toContain("private storage image srcsets");
  });

  test("creates local storage URLs for implicit local bindings", async () => {
    const storages = createStorageBag(
      {
        uploads: {
          provider: "local",
          path: "storage/uploads",
          signing_key: "test-signing-key",
        },
      },
      { now: fixedClock },
    );
    const uploads = requireUploads(storages);

    const url = await uploads.createUploadUrl("avatars/u_123.png");
    const parsed = new URL(url, "https://app.test");

    expect(parsed.pathname).toBe("/_tako/storages/uploads/avatars/u_123.png");
    expect(parsed.searchParams.get("expires")).toBe(
      String(Math.floor(fixedClock().getTime() / 1000) + 3600),
    );
    expect(parsed.searchParams.get("token")).toMatch(/^[0-9a-f]{64}$/);
  });
});
