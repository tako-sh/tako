import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import type { Server } from "node:net";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { Channel, setChannelSocketPublisher } from "../src/channels";
import { expectAsyncToThrow } from "./assertions";
import {
  APP_NAME_ENV,
  assertInternalSocketEnvConsistency,
  callInternal,
  installChannelSocketPublisherFromEnv,
  INTERNAL_SOCKET_ENV,
  internalSocketFromEnv,
  TakoError,
} from "../src/tako/socket";

function clearEnv(): void {
  delete process.env[INTERNAL_SOCKET_ENV];
  delete process.env[APP_NAME_ENV];
}

describe("internalSocketFromEnv", () => {
  beforeEach(clearEnv);
  afterEach(clearEnv);

  test("returns null when neither env var is set", () => {
    expect(internalSocketFromEnv()).toBeNull();
  });

  test("returns null when only socket is set", () => {
    process.env[INTERNAL_SOCKET_ENV] = "/tmp/tako.sock";
    expect(internalSocketFromEnv()).toBeNull();
  });

  test("returns null when only app is set", () => {
    process.env[APP_NAME_ENV] = "demo";
    expect(internalSocketFromEnv()).toBeNull();
  });

  test("returns the pair when both are set", () => {
    process.env[INTERNAL_SOCKET_ENV] = "/tmp/tako.sock";
    process.env[APP_NAME_ENV] = "demo";
    expect(internalSocketFromEnv()).toEqual({
      socketPath: "/tmp/tako.sock",
      app: "demo",
    });
  });
});

describe("assertInternalSocketEnvConsistency", () => {
  beforeEach(clearEnv);
  afterEach(clearEnv);

  test("passes when both env vars are set", () => {
    process.env[INTERNAL_SOCKET_ENV] = "/tmp/tako.sock";
    process.env[APP_NAME_ENV] = "demo";
    expect(() => {
      assertInternalSocketEnvConsistency();
    }).not.toThrow();
  });

  test("passes when neither env var is set (app running outside Tako)", () => {
    expect(() => {
      assertInternalSocketEnvConsistency();
    }).not.toThrow();
  });

  test("throws when only TAKO_INTERNAL_SOCKET is set — TAKO_APP_NAME missing means RPCs can't route", () => {
    process.env[INTERNAL_SOCKET_ENV] = "/tmp/tako.sock";
    expect(() => {
      assertInternalSocketEnvConsistency();
    }).toThrow(/TAKO_APP_NAME/);
  });

  test("throws when only TAKO_APP_NAME is set — missing socket means workflows/channels have nowhere to send", () => {
    process.env[APP_NAME_ENV] = "demo";
    expect(() => {
      assertInternalSocketEnvConsistency();
    }).toThrow(/TAKO_INTERNAL_SOCKET/);
  });
});

describe("callInternal error wrapping", () => {
  let dir: string;

  beforeEach(async () => {
    dir = await mkdtemp(join("/tmp", "tako-sock-err-"));
  });

  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  test("maps a missing unix socket to TakoError TAKO_UNAVAILABLE without leaking the path", async () => {
    const missing = join(dir, "nonexistent.sock");
    let caught: unknown;
    try {
      await callInternal(missing, { command: "noop" });
    } catch (err) {
      caught = err;
    }
    expect(caught).toBeInstanceOf(TakoError);
    const err = caught as TakoError;
    expect(err.code).toBe("TAKO_UNAVAILABLE");
    expect(err.message).not.toContain(missing);
    expect(err.message).not.toContain("ENOENT");
    expect(err.message).not.toContain("connect");
    // Message is brand-neutral — apps can surface it directly without
    // leaking "Tako" to end users.
    expect(err.message).not.toContain("Tako");
    expect(err.message).toBe("Internal Server Error");
    // Original error is preserved for operators on .cause.
    expect(err.cause).toBeDefined();
  });

  test("maps a server error response to TakoError TAKO_RPC_ERROR without leaking the server message", async () => {
    const sock = join(dir, "srv.sock");
    const server = await new Promise<Server>((resolve, reject) => {
      const s = createServer((socket) => {
        socket.on("data", () => {
          socket.write(`${JSON.stringify({ status: "error", message: "unknown workflow 'x'" })}\n`);
        });
      });
      s.once("error", reject);
      s.listen(sock, () => resolve(s));
    });
    try {
      let caught: unknown;
      try {
        await callInternal(sock, { command: "enqueue_run" });
      } catch (err) {
        caught = err;
      }
      expect(caught).toBeInstanceOf(TakoError);
      const err = caught as TakoError;
      expect(err.code).toBe("TAKO_RPC_ERROR");
      expect(err.message).toBe("Internal Server Error");
      expect(err.cause).toBeInstanceOf(Error);
      expect((err.cause as Error).message).toBe("unknown workflow 'x'");
    } finally {
      server.close();
    }
  });
});

describe("installChannelSocketPublisherFromEnv", () => {
  let dir: string;
  let server: Server | null = null;

  beforeEach(async () => {
    clearEnv();
    setChannelSocketPublisher(null);
    dir = await mkdtemp(join("/tmp", "tako-chan-sock-"));
  });

  afterEach(async () => {
    clearEnv();
    setChannelSocketPublisher(null);
    if (server) {
      server.close();
      server = null;
    }
    await rm(dir, { recursive: true, force: true });
  });

  test("returns false when env is missing and does not install a publisher", () => {
    expect(installChannelSocketPublisherFromEnv()).toBe(false);
  });

  test("routes Channel.publish through the internal socket when env is set", async () => {
    const sock = join(dir, "chan.sock");
    let receivedLine = "";
    server = await new Promise<Server>((resolve, reject) => {
      const s = createServer((socket) => {
        socket.on("data", (chunk: Buffer) => {
          receivedLine += chunk.toString("utf8");
          const nl = receivedLine.indexOf("\n");
          if (nl === -1) return;
          socket.write(
            `${JSON.stringify({
              status: "ok",
              data: {
                id: "99",
                channel: "chat/room-1",
                type: "message",
                data: { text: "hi" },
              },
            })}\n`,
          );
        });
      });
      s.once("error", reject);
      s.listen(sock, () => resolve(s));
    });

    process.env[INTERNAL_SOCKET_ENV] = sock;
    process.env[APP_NAME_ENV] = "demo";

    expect(installChannelSocketPublisherFromEnv()).toBe(true);

    const channel = new Channel("chat/room-1");
    const result = await channel.publish({ type: "message", data: { text: "hi" } });

    const parsed = JSON.parse(receivedLine.trim());
    expect(parsed).toEqual({
      command: "channel_publish",
      app: "demo",
      channel: "chat/room-1",
      payload: { type: "message", data: { text: "hi" } },
    });
    expect(result).toEqual({
      id: "99",
      channel: "chat/room-1",
      type: "message",
      data: { text: "hi" },
    });
  });

  test("explicit baseUrl bypasses the installed socket publisher and rejects browser publish", async () => {
    process.env[INTERNAL_SOCKET_ENV] = join(dir, "nonexistent.sock");
    process.env[APP_NAME_ENV] = "demo";
    expect(installChannelSocketPublisherFromEnv()).toBe(true);

    const channel = new Channel("chat/room-1");
    await expectAsyncToThrow(
      () =>
        channel.publish(
          { type: "message", data: { text: "hi" } },
          { baseUrl: "https://app.example.com" },
        ),
      /connect\(\)\.send/,
    );
  });
});
