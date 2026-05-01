import { beforeEach, describe, expect, test } from "bun:test";
import {
  TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH,
  TAKO_INTERNAL_CHANNELS_DISPATCH_PATH,
  TAKO_INTERNAL_CHANNELS_REGISTRY_PATH,
  TAKO_INTERNAL_TOKEN_HEADER,
  handleTakoEndpoint,
} from "../src/tako/endpoints";
import { injectBootstrap } from "../src/tako/secrets";
import type { TakoStatus } from "../src/types";
import { ChannelRegistry } from "../src/channels";
import { defineChannel } from "../src/channels/define";

describe("handleTakoEndpoint", () => {
  injectBootstrap({ token: "test-token", secrets: {} });

  let channels: ChannelRegistry;
  beforeEach(() => {
    channels = new ChannelRegistry();
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

  describe("internal host /status", () => {
    test("returns status JSON", async () => {
      const request = new Request("http://tako.internal/status", {
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
      const request = new Request("http://tako.internal/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, unhealthyStatus, channels);

      const body = await response!.json();
      expect(body.status).toBe("draining");
    });

    test("returns 403 without the internal token header", async () => {
      const request = new Request("http://tako.internal/status");
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(403);
    });

    test("returns status for internal host with explicit port", async () => {
      const request = new Request("http://tako.internal:3000/status", {
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);

      expect(response).not.toBeNull();
      expect(response!.status).toBe(200);
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
      const request = new Request("http://tako.internal/unknown", {
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
        defineChannel({
          name: "chat",
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

      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
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
        replayWindowMs: 86_400_000,
        inactivityTtlMs: 0,
        keepaliveIntervalMs: 25_000,
        maxConnectionLifetimeMs: 7_200_000,
        subject: "user-123",
      });
    });

    test("returns 403 when channel auth denies access", async () => {
      channels.register(
        "chat",
        defineChannel({
          name: "chat",
          auth: {
            verify() {
              return false;
            },
          },
        }),
      );

      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
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
      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
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
        defineChannel({
          name: "chat",
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

      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH}`, {
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
        defineChannel({
          name: "chat",
          auth: { verify: async () => true },
          handler: { msg: async (data: { text: string }) => ({ text: data.text.toUpperCase() }) },
        }),
      );

      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_DISPATCH_PATH}`, {
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
      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_DISPATCH_PATH}`, {
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
      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_DISPATCH_PATH}`, {
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
        defineChannel({
          name: "chat",
          paramsSchema: (t) => t.Object({ roomId: t.String() }),
          auth: { cookieName: "session", verify: async () => true },
          handler: { msg: async (data: { text: string }) => data },
        }),
      );
      channels.register("status", defineChannel({ name: "status" }));

      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_REGISTRY_PATH}`, {
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
      const request = new Request(`http://tako.internal${TAKO_INTERNAL_CHANNELS_REGISTRY_PATH}`, {
        method: "POST",
        headers: { [TAKO_INTERNAL_TOKEN_HEADER]: "test-token" },
      });
      const response = await handleTakoEndpoint(request, mockStatus, channels);
      expect(response!.status).toBe(405);
    });
  });
});
