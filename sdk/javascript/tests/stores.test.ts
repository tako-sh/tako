import { afterEach, expect, test } from "bun:test";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { ChannelRegistry, setChannelSocketPublisher } from "../src/channels";
import { bootstrapChannels } from "../src/channels/bootstrap";
import { defineStore } from "../src/stores/define";
import { StoreRegistry } from "../src/stores/registry";
import { discoverStores } from "../src/stores/discovery";
import { handleTakoEndpoint } from "../src/tako/endpoints";
import { injectBootstrap } from "../src/tako/secrets";
import type { TakoStatus } from "../src/types";

const status: TakoStatus = {
  status: "healthy",
  app: "stores",
  version: "1",
  instance_id: "1",
  pid: 1,
  uptime_seconds: 0,
};
afterEach(() => setChannelSocketPublisher(null));

test("store subscriptions authorize validated params and a trusted identity using its public name", async () => {
  const store = defineStore("messages", {
    paramsSchema: (t) => t.Object({ chatId: t.String() }),
    auth: {
      cookieName: "session",
      headerName: false,
      verify: (input) => {
        expect(input.store).toBe("messages");
        expect(input.params.chatId).toBe("private");
        return input.cookie === "valid" ? { subject: "user-1" } : false;
      },
    },
    load: async ({ params, subject }) => ({ chatId: params.chatId, user: subject }),
  });
  const channels = new ChannelRegistry();
  channels.register(store.definition.channel, store.definition);
  const input = {
    channel: store.definition.channel,
    operation: "subscribe" as const,
    params: { chatId: "private" },
  };
  expect((await channels.authorize(input)).ok).toBe(false);
  expect(await channels.authorize({ ...input, cookie: "valid" })).toMatchObject({
    ok: true,
    subject: "user-1",
  });
  expect((await channels.authorize({ ...input, operation: "publish", cookie: "valid" })).ok).toBe(
    false,
  );
  expect((await channels.authorize({ ...input, operation: "connect", cookie: "valid" })).ok).toBe(
    false,
  );
});

test("invalidation publishes only a scoped notification, validates params, and propagates socket failure", async () => {
  const store = defineStore("messages", {
    paramsSchema: (t) => t.Object({ chatId: t.String() }),
    load: () => [],
  });
  const calls: unknown[] = [];
  setChannelSocketPublisher(async (channel, payload) => {
    calls.push({ channel, payload });
    return { id: "1", channel, type: payload.type, data: payload.data };
  });
  await store.invalidate({ chatId: "private" });
  expect(calls).toEqual([
    {
      channel: "tako.store:messages",
      payload: { type: "tako.store.invalidate", data: {}, scopeParams: { chatId: "private" } },
    },
  ]);
  await expect(store.invalidate({ chatId: 3 } as never)).rejects.toThrow("Invalid params");
  await expect(store.invalidate({ chatId: "x".repeat(16 * 1024) })).rejects.toThrow("16 KiB");
  expect(calls).toHaveLength(1);
  setChannelSocketPublisher(async () => {
    throw new Error("unavailable");
  });
  await expect(store.invalidate({ chatId: "private" })).rejects.toThrow("unavailable");
});

test("internal loader requires the bootstrap token and forwards only trusted params and subject", async () => {
  injectBootstrap({ token: "store-test-token", secrets: {}, storages: {} });
  const stores = new StoreRegistry();
  let calls = 0;
  const store = defineStore("messages", {
    offline: { maxAgeMs: 60_000 },
    load: ({ params, subject, signal }) => {
      calls++;
      expect(signal).toBeInstanceOf(AbortSignal);
      return { params, subject };
    },
  });
  stores.register(store);
  const request = (token?: string, host = "localhost") =>
    new Request(`http://${host}/stores/load`, {
      method: "POST",
      headers: token ? { "x-tako-internal-token": token } : {},
      body: JSON.stringify({
        store: "messages",
        params: { chatId: "private" },
        subject: "verified-user",
      }),
    });
  expect(
    await handleTakoEndpoint(
      request("store-test-token", "public.example"),
      status,
      undefined,
      stores,
    ),
  ).toBeNull();
  expect((await handleTakoEndpoint(request(), status, undefined, stores))!.status).toBe(403);
  expect((await handleTakoEndpoint(request("wrong"), status, undefined, stores))!.status).toBe(403);
  expect(calls).toBe(0);
  const response = (await handleTakoEndpoint(
    request("store-test-token"),
    status,
    undefined,
    stores,
  ))!;
  expect(response.status).toBe(200);
  expect(response.headers.get("Cache-Control")).toBe("no-store");
  expect(response.headers.get("x-tako-internal-token")).toBe("store-test-token");
  expect(await response.json()).toEqual({
    data: { params: { chatId: "private" }, subject: "verified-user" },
    offline: { maxAgeMs: 60_000, version: "1" },
  });
  expect(calls).toBe(1);
});

test("loader rejects malformed requests and sanitizes failure and invalid or oversized results", async () => {
  injectBootstrap({ token: "store-test-token", secrets: {}, storages: {} });
  const stores = new StoreRegistry();
  for (const [name, load] of Object.entries({
    throws: () => {
      throw new Error("database-password-private");
    },
    invalid: () => undefined,
    function: () => () => "invalid",
    large: () => "x".repeat(1024 * 1024),
    circular: () => {
      const data: { self?: unknown } = {};
      data.self = data;
      return data;
    },
  }))
    stores.register(defineStore(name, { load }));
  const call = (body: string, method = "POST") =>
    handleTakoEndpoint(
      new Request("http://localhost/stores/load", {
        method,
        headers: { "x-tako-internal-token": "store-test-token" },
        ...(method === "POST" && { body }),
      }),
      status,
      undefined,
      stores,
    );
  for (const body of [
    "{",
    "null",
    "[]",
    '{"store":"throws","params":[]}',
    '{"store":"throws","params":{},"subject":1}',
    " ".repeat(1024 * 1024 + 1),
  ]) {
    expect((await call(body))!.status).toBe(400);
  }
  expect((await call("", "GET"))!.status).toBe(405);
  expect((await call('{"store":"missing","params":{}}'))!.status).toBe(404);
  for (const name of ["throws", "invalid", "function", "large", "circular"]) {
    const response = (await call(JSON.stringify({ store: name, params: {} })))!;
    expect(response.status).toBe(500);
    expect(await response.json()).toEqual({ error: "Internal Server Error" });
  }
});

test("discovery registers Store metadata and loaders under the configured app root without executing callbacks", async () => {
  const dir = await mkdtemp(join(tmpdir(), "tako-stores-"));
  try {
    await mkdir(join(dir, "app/stores"), { recursive: true });
    const source = JSON.stringify(resolve(import.meta.dir, "../src/stores/define.ts"));
    await writeFile(
      join(dir, "app/stores/messages.ts"),
      `import {defineStore} from ${source}; export default defineStore("chat-messages", {load:()=>{throw new Error("never during discovery")}});`,
    );
    const result = await bootstrapChannels({ appDir: dir, appRoot: "app" });
    expect(result.channelCount).toBe(1);
    expect(result.registry.all.map((e) => e.name)).toEqual(["tako.store:chat-messages"]);
    expect(result.stores.find("chat-messages")?.name).toBe("chat-messages");
    await writeFile(
      join(dir, "app/stores/duplicate.ts"),
      `import {defineStore} from ${source}; export default defineStore("chat-messages", {load:()=>null});`,
    );
    await expect(discoverStores(join(dir, "app/stores"))).rejects.toThrow("duplicate store");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("store policy is opt-in and validates names and expiry", () => {
  expect(defineStore("public", { load: () => null }).offline).toBeUndefined();
  for (const name of ["", "with/slash", "with space", "_first", "x".repeat(257)]) {
    expect(() => defineStore(name, { load: () => null })).toThrow("Store names");
  }
  for (const maxAgeMs of [0, -1, Infinity, 0.5, Number.MAX_SAFE_INTEGER + 1]) {
    expect(() => defineStore("test", { load: () => null, offline: { maxAgeMs } })).toThrow(
      "offline policy",
    );
  }
  expect(() =>
    defineStore("test", { load: () => null, offline: { maxAgeMs: 1, version: "" } }),
  ).toThrow("offline policy");
});

test("public Store authorization is read-only even without a verifier", async () => {
  const store = defineStore("public", { load: () => null });
  const registry = new ChannelRegistry();
  registry.register(store.definition.channel, store.definition);
  for (const operation of ["connect", "publish"] as const) {
    expect(
      (await registry.authorize({ channel: store.definition.channel, operation, params: {} })).ok,
    ).toBe(false);
  }
});
