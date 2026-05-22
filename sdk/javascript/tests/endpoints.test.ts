import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH,
  TAKO_INTERNAL_CHANNELS_DISPATCH_PATH,
  TAKO_INTERNAL_CHANNELS_REGISTRY_PATH,
  TAKO_INTERNAL_TOKEN_HEADER,
  handleTakoEndpoint,
} from "../src/tako/endpoints";
import { injectBootstrap } from "../src/tako/secrets";
import { createStorageBag } from "../src/storage";
import type { TakoStatus } from "../src/types";
import { ChannelRegistry } from "../src/channels";
import { defineChannel } from "../src/channels/define";

describe("handleTakoEndpoint", () => {
  const previousAppName = process.env["TAKO_APP_NAME"];
  const previousDataDir = process.env["TAKO_DATA_DIR"];

  let channels: ChannelRegistry;
  beforeEach(() => {
    process.env["TAKO_APP_NAME"] = "test-app";
    injectBootstrap({ token: "test-token", secrets: {}, storages: {} });
    channels = new ChannelRegistry();
  });
  afterEach(() => {
    if (previousAppName === undefined) {
      delete process.env["TAKO_APP_NAME"];
    } else {
      process.env["TAKO_APP_NAME"] = previousAppName;
    }
    if (previousDataDir === undefined) {
      delete process.env["TAKO_DATA_DIR"];
    } else {
      process.env["TAKO_DATA_DIR"] = previousDataDir;
    }
  });

  const mockStatus: TakoStatus = {
    status: "healthy",
    app: "test-app",
    version: "abc123",
    instance_id: "1",
    pid: 12345,
    uptime_seconds: 3600,
  };

  test("returns null for non-internal host even on /status", async () => {
    const request = new Request("http://example.com/status");
    const response = await handleTakoEndpoint(request, mockStatus, channels);
    expect(response).toBeNull();
  });

  test("returns null for non-internal host paths", async () => {
    const request = new Request("http://example.com/api/users");
    const response = await handleTakoEndpoint(request, mockStatus, channels);
    expect(response).toBeNull();
  });

  test("returns null for root path on non-internal host", async () => {
    const request = new Request("http://example.com/");
    const response = await handleTakoEndpoint(request, mockStatus, channels);
    expect(response).toBeNull();
  });

  test("handles signed local storage upload and download URLs on public hosts", async () => {
    const dataDir = await mkdtemp(join(tmpdir(), "tako-local-storage-"));
    process.env["TAKO_DATA_DIR"] = dataDir;
    const localBinding = {
      provider: "local",
      path: "storage/uploads",
      signing_key: "test-signing-key",
    } as const;
    injectBootstrap({
      token: "test-token",
      secrets: {},
      storages: { uploads: localBinding },
    });
    const storage = createStorageBag({ uploads: localBinding }).uploads;
    if (!storage) throw new Error("missing local storage");

    const uploadUrl = await storage.createUploadUrl("avatars/u_123.txt");
    const upload = await handleTakoEndpoint(
      new Request(new URL(uploadUrl, "http://example.com"), {
        method: "PUT",
        body: "image-bytes",
      }),
      mockStatus,
      channels,
    );

    expect(upload!.status).toBe(204);
    expect(await readFile(join(dataDir, "storage/uploads/avatars/u_123.txt"), "utf8")).toBe(
      "image-bytes",
    );

    const downloadUrl = await storage.createDownloadUrl("avatars/u_123.txt");
    const download = await handleTakoEndpoint(
      new Request(new URL(downloadUrl, "http://example.com")),
      mockStatus,
      channels,
    );

    expect(download!.status).toBe(200);
    expect(await download!.text()).toBe("image-bytes");
  });

  test("rejects local storage URLs signed for another method", async () => {
    const dataDir = await mkdtemp(join(tmpdir(), "tako-local-storage-"));
    process.env["TAKO_DATA_DIR"] = dataDir;
    const localBinding = {
      provider: "local",
      path: "storage/uploads",
      signing_key: "test-signing-key",
    } as const;
    injectBootstrap({
      token: "test-token",
      secrets: {},
      storages: { uploads: localBinding },
    });
    const storage = createStorageBag({ uploads: localBinding }).uploads;
    if (!storage) throw new Error("missing local storage");

    const uploadUrl = await storage.createUploadUrl("avatars/u_123.txt");
    const response = await handleTakoEndpoint(
      new Request(new URL(uploadUrl, "http://example.com")),
      mockStatus,
      channels,
    );

    expect(response!.status).toBe(403);
    expect(await response!.json()).toEqual({ error: "Forbidden" });
  });

  test("rejects malformed local storage URLs without throwing", async () => {
    const response = await handleTakoEndpoint(
      new Request("http://example.com/_tako/storages/%E0%A4%A/file.txt"),
      mockStatus,
      channels,
    );

    expect(response!.status).toBe(400);
    expect(await response!.json()).toEqual({ error: "Invalid key" });
  });

  describe("internal host /status", () => {
    test("returns status JSON", async () => {
      const request = new Request("http://test-app.tako/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
      expect(response!.headers.get("Content-Type")).toBe("application/json");
      expect(response!.headers.get(TAKO_INTERNAL_TOKEN_HEADER)).toBe("test-token");

      const body = await response!.json();
      expect(body).toEqual(mockStatus);
    });

    test("returns current status value", async () => {
      const unhealthyStatus: TakoStatus = {
        ...mockStatus,
        status: "draining",
      };
      const request = new Request("http://test-app.tako/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, unhealthyStatus, channels);

      const body = await response!.json();
      expect(body.status).toBe("draining");
    });

    test("returns 403 without the internal token header", async () => {
      const request = new Request("http://test-app.tako/status");
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(403);
    });

    test("returns status for internal host with explicit port", async () => {
      const request = new Request("http://test-app.tako:3000/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
    });

    test("uses the base app segment when TAKO_APP_NAME is a deployment id", async () => {
      process.env["TAKO_APP_NAME"] = "test-app/production";
      const request = new Request("http://test-app.tako/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
    });

    test("returns null for a different app-scoped internal host", async () => {
      const request = new Request("http://other-app.tako/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).toBeNull();
    });

    test("returns status for loopback host with valid token", async () => {
      const request = new Request("http://127.0.0.1:3000/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
    });
  });

  describe("internal host unknown paths", () => {
    test("returns 404 for unknown paths on internal host", async () => {
      const request = new Request("http://test-app.tako/unknown", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(404);

      const body = await response!.json();
      expect(body.error).toBe("Not found");
    });
  });

  describe("internal host channel auth", () => {
    test("authorizes a matching channel definition", async () => {
      channels.register(
        "chat",
        defineChannel("chat", {
          auth: {
            verify(input) {
              expect(input.header).toEqual({ scheme: "Bearer", value: "test" });
              expect(input.params).toEqual({ roomId: "room-123" });
              expect(input.channel).toBe("chat");
              expect(input.operation).toBe("subscribe");
              return { subject: "user-123" };
            },
          },
        }),
      );

      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [TAKO_INTERNAL_TOKEN_HEADER]: "test-token",
        },
        body: JSON.stringify({
          channel: "chat",
          operation: "subscribe",
          params: { roomId: "room-123" },
          header: {
            scheme: "Bearer",
            value: "test",
          },
        }),
      });

      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
      expect(await response!.json()).toEqual({
        ok: true,
        replayWindowMs: 600_000,
        inactivityTtlMs: 0,
        keepaliveIntervalMs: 25_000,
        maxConnectionLifetimeMs: 7_200_000,
        subject: "user-123",
      });
    });

    test("returns 403 when channel auth denies access", async () => {
      channels.register(
        "chat",
        defineChannel("chat", {
          auth: {
            verify() {
              return false;
            },
          },
        }),
      );

      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [TAKO_INTERNAL_TOKEN_HEADER]: "test-token",
        },
        body: JSON.stringify({
          channel: "chat",
          operation: "subscribe",
          params: { roomId: "room-123" },
        }),
      });

      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response).not.toBeNull();
      expect(response!.status).toBe(403);
      expect(await response!.json()).toEqual({
        error: "Forbidden",
        ok: false,
      });
    });

    test("returns 404 when no channel definition matches", async () => {
      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [TAKO_INTERNAL_TOKEN_HEADER]: "test-token",
        },
        body: JSON.stringify({
          channel: "chat:room-123",
          operation: "publish",
          params: {},
        }),
      });

      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response).not.toBeNull();
      expect(response!.status).toBe(404);
      expect(await response!.json()).toEqual({
        error: "Channel not defined",
        ok: false,
      });
    });

    test("returns channel lifecycle config in authorize responses", async () => {
      channels.register(
        "chat",
        defineChannel("chat", {
          auth: {
            verify() {
              return { subject: "user-123" };
            },
          },
          handler: { msg: async (d: { text: string }) => d },
          replayWindowMs: 86_400_000,
          inactivityTtlMs: 0,
          keepaliveIntervalMs: 25_000,
          maxConnectionLifetimeMs: 7_200_000,
        }),
      );

      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [TAKO_INTERNAL_TOKEN_HEADER]: "test-token",
        },
        body: JSON.stringify({
          channel: "chat",
          operation: "subscribe",
          params: { roomId: "room-123" },
        }),
      });

      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
      expect(await response!.json()).toEqual({
        ok: true,
        subject: "user-123",
        replayWindowMs: 86_400_000,
        inactivityTtlMs: 0,
        keepaliveIntervalMs: 25_000,
        maxConnectionLifetimeMs: 7_200_000,
        transport: "ws",
      });
    });
  });

  describe("internal host channel dispatch", () => {
    test("returns fanout data for a handled type", async () => {
      channels.register(
        "chat",
        defineChannel("chat", {
          auth: { verify: async () => true },
          handler: { msg: async (data: { text: string }) => ({ text: data.text.toUpperCase() }) },
        }),
      );

      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_DISPATCH_PATH}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [TAKO_INTERNAL_TOKEN_HEADER]: "test-token",
        },
        body: JSON.stringify({
          channel: "chat",
          params: { roomId: "r1" },
          frame: { type: "msg", data: { text: "hi" } },
          subject: "u1",
        }),
      });

      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
      expect(await response!.json()).toEqual({
        action: "fanout",
        data: { text: "HI" },
      });
    });

    test("returns reject for unknown channel", async () => {
      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_DISPATCH_PATH}`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          [TAKO_INTERNAL_TOKEN_HEADER]: "test-token",
        },
        body: JSON.stringify({
          channel: "nope",
          frame: { type: "msg", data: {} },
        }),
      });

      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response!.status).toBe(200);
      expect(await response!.json()).toEqual({
        action: "reject",
        reason: "channel_not_defined",
      });
    });

    test("rejects non-POST methods", async () => {
      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_DISPATCH_PATH}`, {
        method: "GET",
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response!.status).toBe(405);
    });
  });

  describe("internal host channel registry", () => {
    test("returns channel definition metadata", async () => {
      channels.register(
        "chat",
        defineChannel("chat", {
          paramsSchema: (t) => t.Object({ roomId: t.String() }),
          auth: { cookieName: "session", verify: async () => true },
          handler: { msg: async (data: { text: string }) => data },
        }),
      );
      channels.register("status", defineChannel("status"));

      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_REGISTRY_PATH}`, {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response!.status).toBe(200);
      expect(await response!.json()).toEqual([
        {
          channel: "chat",
          paramsSchema: {
            type: "object",
            properties: { roomId: { type: "string" } },
            required: ["roomId"],
          },
          auth: { headerName: "authorization", cookieName: "session" },
          transport: "ws",
        },
        {
          channel: "status",
          paramsSchema: { type: "object", properties: {} },
          auth: false,
        },
      ]);
    });

    test("rejects non-GET methods", async () => {
      const request = new Request(`http://test-app.tako${TAKO_INTERNAL_CHANNELS_REGISTRY_PATH}`, {
        method: "POST",
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response!.status).toBe(405);
    });
  });
});
