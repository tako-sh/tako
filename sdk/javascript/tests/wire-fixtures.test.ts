/**
 * Pins the SDK's internal-socket requests and reply parsing to the wire
 * fixtures `tako-core` owns in `test-support/wire/` (see its README).
 */
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import type { Server } from "node:net";
import { join } from "node:path";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test } from "bun:test";
import { setChannelSocketCloser, setChannelSocketPublisher } from "../src/channels";
import { defineChannel } from "../src/channels/define";
import { call, channel, setNetworkCaller, workflow } from "../src/network";
import { defineStore } from "../src/stores/define";
import { initBootstrapFromFd } from "../src/tako/secrets-fd";
import {
  getInternalToken,
  getStorageBindings,
  injectBootstrap,
  loadSecrets,
} from "../src/tako/secrets";
import {
  APP_NAME_ENV,
  BUILD_ENV,
  closeInternalSocketPoolsForTests,
  installChannelSocketPublisherFromEnv,
  INTERNAL_SOCKET_ENV,
  TakoError,
} from "../src/tako/socket";
import { isStaleOwnerError, WorkflowsClient } from "../src/workflows/rpc-client";

const WIRE_DIR = join(import.meta.dir, "../../../test-support/wire");
if (!existsSync(WIRE_DIR)) {
  throw new Error(
    `Wire fixtures not found at ${WIRE_DIR}; run the SDK tests from a full checkout.`,
  );
}

function fixture(path: string): unknown {
  return JSON.parse(readFileSync(join(WIRE_DIR, path), "utf8"));
}

const APP = "demo";
const RUN_ID = "run-1";
const CLAIM = "claim-7";
/** 2026-01-01T00:00:00Z, the instant the fixtures are built around. */
const AT_MS = 1_767_225_600_000;

/** A fake internal socket: records each command and answers with queued replies. */
class FakeSocket {
  readonly received: unknown[] = [];
  private readonly replies: unknown[] = [];
  private server: Server | null = null;
  private dir = "";
  path = "";

  async start(): Promise<void> {
    this.dir = await mkdtemp(join("/tmp", "tako-wire-"));
    this.path = join(this.dir, "internal.sock");
    this.server = createServer((socket) => {
      let buffered = "";
      socket.on("data", (chunk: Buffer) => {
        buffered += chunk.toString("utf8");
        for (let nl = buffered.indexOf("\n"); nl !== -1; nl = buffered.indexOf("\n")) {
          this.received.push(JSON.parse(buffered.slice(0, nl)));
          buffered = buffered.slice(nl + 1);
          const reply = this.replies.shift() ?? fixture("replies/ok.json");
          socket.write(`${JSON.stringify(reply)}\n`);
        }
      });
    });
    await new Promise<void>((resolve, reject) => {
      this.server?.once("error", reject);
      this.server?.listen(this.path, resolve);
    });
  }

  async stop(): Promise<void> {
    this.server?.close();
    await rm(this.dir, { recursive: true, force: true });
  }

  reply(path: string): void {
    this.replies.push(fixture(path));
  }

  /** The one command sent since the last call. */
  take(): unknown {
    expect(this.received).toHaveLength(1);
    return this.received.shift();
  }
}

const socket = new FakeSocket();
let client: WorkflowsClient;

beforeAll(async () => {
  await socket.start();
  client = new WorkflowsClient(socket.path, APP);
});

afterAll(async () => {
  closeInternalSocketPoolsForTests();
  await socket.stop();
});

describe("workflow commands match the wire fixtures", () => {
  let previousBuild: string | undefined;
  beforeEach(() => {
    previousBuild = process.env[BUILD_ENV];
    process.env[BUILD_ENV] = "v7";
  });
  afterEach(() => {
    if (previousBuild === undefined) delete process.env[BUILD_ENV];
    else process.env[BUILD_ENV] = previousBuild;
  });
  const cases: [string, () => Promise<unknown>][] = [
    [
      "enqueue_run",
      () =>
        client.enqueue(
          "send-email",
          { to: "a@example.com" },
          { runAt: new Date(AT_MS), retries: 4, id: "order-42" },
        ),
    ],
    ["enqueue_run-defaults", () => client.enqueue("send-email", { to: "a@example.com" })],
    ["enqueue_run-null_payload", () => client.enqueue("send-email", null)],
    ["get_run_result", () => client.result("send-email", "order-42")],
    [
      "register_schedules",
      () =>
        client.registerSchedules([
          { name: "nightly-report", cron: "0 3 * * *", max_attempts: 5 },
          { name: "cleanup", cron: "*/15 * * * *" },
        ]),
    ],
    ["register_schedules-empty", () => client.registerSchedules([])],
    ["claim_run", () => client.claim("worker-1", ["send-email", "nightly-report"], 30_000)],
    ["heartbeat_run", () => client.heartbeat(RUN_ID, CLAIM, 30_000)],
    ["save_step", () => client.saveStep(RUN_ID, CLAIM, "fetch-user", { id: "u1" })],
    ["complete_run", () => client.complete(RUN_ID, CLAIM, { sent: true })],
    ["cancel_run", () => client.cancel(RUN_ID, CLAIM, "customer asked")],
    ["cancel_run-no_reason", () => client.cancel(RUN_ID, CLAIM)],
    [
      "fail_run-retry",
      () => client.fail(RUN_ID, CLAIM, "smtp timeout", new Date(AT_MS + 60_000), false),
    ],
    ["fail_run-finalize", () => client.fail(RUN_ID, CLAIM, "smtp timeout", null, true)],
    ["defer_run", () => client.defer(RUN_ID, CLAIM, new Date(AT_MS + 3_600_000))],
    ["defer_run-indefinite", () => client.defer(RUN_ID, CLAIM, null)],
    [
      "wait_for_event",
      () =>
        client.waitForEvent(
          RUN_ID,
          CLAIM,
          "approval",
          "order-approved",
          new Date(AT_MS + 86_400_000),
        ),
    ],
    [
      "wait_for_event-no_timeout",
      () => client.waitForEvent(RUN_ID, CLAIM, "approval", "order-approved", null),
    ],
    ["signal", () => client.signal("order-approved", { orderId: "42" })],
  ];

  for (const [name, send] of cases) {
    test(name, async () => {
      // Replies the call can parse; the shapes are checked below.
      if (name.startsWith("enqueue_run")) socket.reply("replies/enqueue_run.json");
      if (name === "signal") socket.reply("replies/signal.json");
      if (name === "claim_run" || name === "get_run_result") socket.reply("replies/ok-null.json");
      await send();
      expect(socket.take()).toEqual(fixture(`commands/${name}.json`));
    });
  }
});

describe("channel, store, and network commands match the wire fixtures", () => {
  beforeEach(() => {
    process.env[INTERNAL_SOCKET_ENV] = socket.path;
    process.env[APP_NAME_ENV] = APP;
    expect(installChannelSocketPublisherFromEnv()).toBe(true);
  });

  afterEach(() => {
    delete process.env[INTERNAL_SOCKET_ENV];
    delete process.env[APP_NAME_ENV];
    delete process.env[BUILD_ENV];
    setChannelSocketPublisher(null);
    setChannelSocketCloser(null);
    setNetworkCaller(null);
  });

  test("channel_publish", async () => {
    process.env[BUILD_ENV] = "v7";
    const board = defineChannel("board", {
      paramsSchema: (t) => t.Object({ base: t.String(), page: t.Integer() }),
    });
    await board({ base: "north", page: 2 }).publish({ type: "message", data: { text: "hi" } });
    expect(socket.take()).toEqual(fixture("commands/channel_publish.json"));
  });

  test("channel_publish-store_invalidate", async () => {
    const messages = defineStore("messages", {
      paramsSchema: (t) => t.Object({ chatId: t.String() }),
      load: () => [],
    });
    await messages.invalidate({ chatId: "chat-a" });
    expect(socket.take()).toEqual(fixture("commands/channel_publish-store_invalidate.json"));
    process.env[BUILD_ENV] = "v7";
    await messages.invalidate({ chatId: "chat-a" });
    expect(socket.take()).toEqual(fixture("commands/channel_publish-store_invalidate-build.json"));
  });

  test("channel_close", async () => {
    process.env[BUILD_ENV] = "v7";
    const reply = defineChannel("reply", {
      paramsSchema: (t) => t.Object({ requestId: t.String() }),
    });
    await reply({ requestId: "r1" }).close();
    expect(socket.take()).toEqual(fixture("commands/channel_close.json"));
  });

  test("network_call-call", async () => {
    await call("users.lookup", { id: "u1" });
    expect(socket.take()).toEqual(fixture("commands/network_call-call.json"));
  });

  test("network_call-result", async () => {
    socket.reply("replies/run_result-succeeded.json");
    expect(await workflow("mail.send-email").result(RUN_ID)).toEqual({ sent: true });
    expect(socket.take()).toEqual(fixture("commands/network_call-result.json"));
  });

  test("network_call-enqueue", async () => {
    socket.reply("replies/enqueue_run.json");
    const id = await workflow("mail.send-email").enqueue(
      { to: "a@example.com" },
      { runAt: new Date(AT_MS), retries: 4, id: "order-42" },
    );
    expect(id).toBe("order-42");
    expect(socket.take()).toEqual(fixture("commands/network_call-enqueue.json"));
  });

  test("network_call-publish", async () => {
    await channel("chat.messages", { room: "lobby" }).publish({
      type: "message",
      data: { text: "hi" },
    });
    expect(socket.take()).toEqual(fixture("commands/network_call-publish.json"));
  });
});

describe("reply fixtures parse to the expected values", () => {
  afterEach(() => {
    socket.received.length = 0;
  });

  test("claim_run returns the run with its claim owner", async () => {
    socket.reply("replies/claim_run.json");
    expect(await client.claim("worker-1", ["send-email"], 30_000)).toEqual({
      id: RUN_ID,
      name: "send-email",
      payload: { to: "a@example.com" },
      status: "running",
      attempts: 2,
      retries: 4,
      runAt: AT_MS,
      leaseUntil: null,
      workerId: CLAIM,
      lastError: null,
      stepState: { "fetch-user": { id: "u1" } },
      createdAt: 0,
    });
  });

  test("a null claim_run reply means nothing is due", async () => {
    socket.reply("replies/ok-null.json");
    expect(await client.claim("worker-1", ["send-email"], 30_000)).toBeNull();
  });

  test("enqueue_run returns the id and dedup flag", async () => {
    socket.reply("replies/enqueue_run.json");
    expect(await client.enqueue("send-email", null)).toEqual({
      id: "order-42",
      deduplicated: true,
    });
  });

  test("get_run_result returns the retained outcome, or null", async () => {
    socket.reply("replies/run_result-succeeded.json");
    socket.reply("replies/run_result-dead.json");
    socket.reply("replies/ok-null.json");
    expect(await client.result("send-email", RUN_ID)).toEqual({
      status: "succeeded",
      result: { sent: true },
      error: null,
    });
    expect(await client.result("send-email", RUN_ID)).toEqual({
      status: "dead",
      result: null,
      error: "smtp timeout",
    });
    expect(await client.result("send-email", RUN_ID)).toBeNull();
  });

  test("signal returns the number of woken runs", async () => {
    socket.reply("replies/signal.json");
    expect(await client.signal("order-approved", null)).toBe(2);
  });

  test("worker writes resolve on the ok reply", async () => {
    socket.reply("replies/ok.json");
    expect(await client.heartbeat(RUN_ID, CLAIM, 30_000)).toBeUndefined();
  });

  const errorFixtures = readdirSync(join(WIRE_DIR, "replies"))
    .filter((name) => name.startsWith("error"))
    .sort();

  test("every error fixture is covered", () => {
    expect(errorFixtures.length).toBeGreaterThanOrEqual(9);
  });

  for (const name of errorFixtures) {
    test(`${name} maps to its TakoError reason`, async () => {
      const reply = fixture(`replies/${name}`) as { message: string; code?: string };
      // Unclassified and unknown codes carry no reason.
      const reason = name === "error-unknown_code.json" ? undefined : reply.code;
      socket.reply(`replies/${name}`);
      const err = await client.heartbeat(RUN_ID, CLAIM, 30_000).catch((e: unknown) => e);
      expect(err).toBeInstanceOf(TakoError);
      expect((err as TakoError).code).toBe("TAKO_RPC_ERROR");
      expect((err as TakoError).reason).toBe(reason as TakoError["reason"]);
      expect(((err as TakoError).cause as Error).message).toBe(reply.message);
      expect(isStaleOwnerError(err)).toBe(reason === "run_not_owned");
    });
  }
});

describe("bootstrap fixture", () => {
  afterEach(() => {
    injectBootstrap({ token: null, secrets: {} });
  });

  test("populates the token, secrets, and storage bindings", () => {
    const text = readFileSync(join(WIRE_DIR, "bootstrap.json"), "utf8");
    initBootstrapFromFd(() => text);
    const envelope = fixture("bootstrap.json") as { storages: Record<string, unknown> };
    expect(getInternalToken()).toBe("tok-abc");
    const secrets = loadSecrets<Record<string, string>>();
    expect(secrets["DATABASE_URL"]).toBe("postgres://db/app");
    expect(secrets["API_KEY"]).toBe("key-123");
    expect({ ...getStorageBindings() }).toEqual(envelope.storages);
  });
});
