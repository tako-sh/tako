import { afterEach, beforeEach, expect, test } from "bun:test";
import { Channel } from "../../src/channels";
import { configureChannels, resetChannelsConfig } from "../../src/channels/configure";

type Frame = {
  type: string;
  id: string;
  channel?: string;
  query?: string;
  cursor?: string;
  from?: string;
  headers?: Record<string, string>;
  data?: unknown;
};
class Socket extends EventTarget {
  static all: Socket[] = [];
  readonly sent: Frame[] = [];
  readyState = 0;
  bufferedAmount = 0;
  closed = false;
  constructor(readonly url: string) {
    super();
    Socket.all.push(this);
  }
  send(data: string) {
    this.sent.push(JSON.parse(data) as Frame);
  }
  close() {
    this.closed = true;
    this.readyState = 3;
  }
  open() {
    this.readyState = 1;
    this.dispatchEvent(new Event("open"));
  }
  frame(frame: unknown) {
    this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify(frame) }));
  }
  disconnect() {
    this.readyState = 3;
    this.dispatchEvent(Object.assign(new Event("close"), { code: 1006, reason: "lost" }));
  }
  ready(id: string) {
    this.frame({ type: "subscribed", id });
  }
  message(id: string, data: unknown) {
    this.frame({ type: "data", id, data: JSON.stringify(data), end: true });
  }
}

const cleanup: Array<() => void> = [];
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
beforeEach(() => {
  resetChannelsConfig();
  Socket.all = [];
  configureChannels({ websocket: Socket as unknown as typeof WebSocket });
});
afterEach(async () => {
  cleanup.splice(0).forEach((close) => close());
  await tick();
  resetChannelsConfig();
});
function connect(name: string, options: Parameters<Channel["connect"]>[0] = {}) {
  const connection = new Channel(name, "ws").connect({ baseUrl: "https://app.test", ...options });
  cleanup.push(() => connection.close());
  return connection;
}

test("different channels and parameter scopes share a physical socket and release independently", async () => {
  const first = connect("chat");
  const second = new Channel("chat", "ws", { room: "other" }).connect({
    baseUrl: "https://app.test",
  });
  cleanup.push(() => second.close());
  expect(Socket.all).toHaveLength(1);
  const socket = Socket.all[0]!;
  expect(socket.url).toBe("wss://app.test/_tako/channels");
  socket.open();
  await tick();
  const [a, b] = socket.sent;
  expect(a?.id).not.toBe(b?.id);
  expect(b?.query).toBe("room=other");
  const received: unknown[] = [];
  (second.raw as EventTarget).addEventListener("message", (event) =>
    received.push((event as MessageEvent).data),
  );
  socket.ready(a!.id);
  socket.ready(b!.id);
  first.close();
  expect(socket.sent.at(-1)).toEqual({ type: "unsubscribe", id: a!.id });
  expect(socket.closed).toBe(false);
  socket.message(a!.id, { id: "old" });
  socket.message(b!.id, { id: "live" });
  expect(received).toEqual(['{"id":"live"}']);
  second.send({ type: "chat", data: "hello" });
  expect(socket.sent.at(-1)).toEqual({
    type: "send",
    id: b!.id,
    data: { type: "chat", data: "hello" },
  });
  second.close();
  await tick();
  expect(socket.closed).toBe(true);
});

test("origin and credentials are separate connection boundaries", () => {
  connect("a", { authorization: "one" });
  connect("b", { authorization: "two" });
  connect("c", { authorization: "one" });
  connect("d", { authorization: "one", baseUrl: "https://another.test" });
  expect(Socket.all).toHaveLength(3);
});

test("the shared WebSocket default preserves independent SSE subscriptions", () => {
  configureChannels({
    fetch: (async () =>
      new Response("", { headers: { "content-type": "text/event-stream" } })) as typeof fetch,
  });
  const subscription = new Channel("feed").subscribe({ baseUrl: "https://app.test" });
  cleanup.push(subscription.close);
  expect(subscription.transport).toBe("sse");
  expect(Socket.all).toHaveLength(0);
});

test("leaving while asynchronous authentication is pending cannot resurrect the subscription", async () => {
  let resolve!: (token: string) => void;
  configureChannels({
    token: () =>
      new Promise((done) => {
        resolve = done;
      }),
  });
  const first = connect("a");
  const socket = Socket.all[0]!;
  socket.open();
  first.close();
  resolve("Bearer private");
  await tick();
  expect(socket.sent.filter((frame) => frame.type === "subscribe")).toHaveLength(0);
  expect(socket.closed).toBe(true);
});

test("authorization rejection and terminal closure affect only their subscriptions", async () => {
  let closed = 0;
  const first = connect("a", { onClose: () => closed++ });
  const second = connect("b");
  const socket = Socket.all[0]!;
  socket.open();
  await tick();
  const [a, b] = socket.sent;
  socket.ready(a!.id);
  socket.ready(b!.id);
  socket.frame({ type: "closed", id: a!.id, code: 4200, reason: "stream-closed" });
  socket.frame({ type: "closed", id: a!.id, code: 4200, reason: "stream-closed" });
  expect(closed).toBe(1);
  expect(() => first.send({ type: "x", data: 1 })).toThrow();
  expect(() => second.send({ type: "x", data: 1 })).not.toThrow();
  expect(socket.closed).toBe(false);
});

test("interleaved UTF-8 fragments are reassembled per subscription", async () => {
  const first = connect("a");
  const second = connect("b");
  const received: string[] = [];
  for (const channel of [first, second])
    (channel.raw as EventTarget).addEventListener("message", (e) =>
      received.push((e as MessageEvent<string>).data),
    );
  const socket = Socket.all[0]!;
  socket.open();
  await tick();
  const [a, b] = socket.sent;
  socket.ready(a!.id);
  socket.ready(b!.id);
  const body = JSON.stringify({ text: "🦑".repeat(25_000) });
  for (let i = 0; i < body.length; i += 4000) {
    socket.frame({
      type: "data",
      id: a!.id,
      data: body.slice(i, i + 4000),
      end: i + 4000 >= body.length,
    });
    if (i === 0) socket.message(b!.id, { notice: "urgent" });
  }
  expect(received).toEqual(['{"notice":"urgent"}', body]);
});

test("oversized logical messages fail their own subscription and outgoing writes are bounded", async () => {
  const first = connect("a");
  const second = connect("b");
  const socket = Socket.all[0]!;
  socket.open();
  await tick();
  const [a, b] = socket.sent;
  socket.ready(a!.id);
  socket.ready(b!.id);
  for (let i = 0; i < 130; i++)
    socket.frame({ type: "data", id: a!.id, data: "x".repeat(8192), end: false });
  expect((first.raw as { readyState: number }).readyState).toBe(3);
  expect(socket.closed).toBe(false);
  expect(() => second.send({ type: "x", data: "x".repeat(64 * 1024) })).toThrow(/64 KiB/);
  socket.bufferedAmount = 1024 * 1024;
  expect(() => second.send({ type: "x", data: 1 })).toThrow(/queue is full/);
});

test("reopening a logical socket sends only its own replay cursor", async () => {
  const first = connect("a", { from: "start" });
  const second = connect("b", { lastMessageId: "5" });
  const socket = Socket.all[0]!;
  socket.open();
  await tick();
  const [a, b] = socket.sent;
  expect(a?.from).toBe("start");
  expect(b?.cursor).toBe("5");
  socket.ready(a!.id);
  socket.ready(b!.id);
  first.close();
  connect("a", { lastMessageId: "12" });
  await tick();
  expect(Socket.all).toHaveLength(1);
  expect(socket.sent.at(-1)?.cursor).toBe("12");
  expect((second.raw as { readyState: number }).readyState).toBe(1);
});

test("an AbortSignal disposes only its page subscription", async () => {
  const controller = new AbortController();
  const first = connect("a", { signal: controller.signal });
  connect("b");
  const socket = Socket.all[0]!;
  socket.open();
  await tick();
  controller.abort();
  expect((first.raw as { readyState: number }).readyState).toBe(3);
  expect(socket.closed).toBe(false);
});

test("subscription capacity is bounded and releasing a slot permits another page", () => {
  const channels = Array.from({ length: 32 }, (_, index) => connect(`page-${index}`));
  expect(() => connect("overflow")).toThrow(/32 subscriptions/);
  channels[0]!.close();
  expect(() => connect("replacement")).not.toThrow();
  expect(Socket.all).toHaveLength(1);
});

test("join failures on an already-open connection reach the new subscription's listeners", async () => {
  connect("layout");
  const socket = Socket.all[0]!;
  socket.open();
  await tick();
  // A large query shares the existing credential pool but exceeds the command limit.
  const page = new Channel("page", "ws", { value: "x".repeat(65536) }).connect({
    baseUrl: "https://app.test",
  });
  cleanup.push(() => page.close());
  let code: number | undefined;
  (page.raw as EventTarget).addEventListener("close", (event) => {
    code = (event as CloseEvent).code;
  });
  await tick();
  expect(code).toBe(1009);
  expect(socket.closed).toBe(false);
});
