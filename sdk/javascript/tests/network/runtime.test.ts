import { afterEach, expect, test } from "bun:test";
import { channel, workflow, setNetworkCaller } from "../../src/network";
import { ChannelRegistry, setChannelSocketPublisher } from "../../src/channels";
import { defineChannel } from "../../src/channels/define";
import { handleNetworkInvocation } from "../../src/network-endpoint";

afterEach(() => {
  setNetworkCaller(null);
  setChannelSocketPublisher(null);
});

test("string handles preserve addresses and use the existing enqueue wire options", async () => {
  const calls: unknown[] = [];
  setNetworkCaller(async (address, request) => {
    calls.push({ address, request });
    return { id: "run-1" };
  });
  expect(
    await workflow("mail.send-email").enqueue(
      { to: "a@example.com" },
      { retries: 4, runAt: new Date(1234), id: "stable-id" },
    ),
  ).toBe("run-1");
  await channel("web.progress", { jobId: "job-1" }).publish({
    type: "update",
    data: { percent: 50 },
  });
  expect(calls).toEqual([
    {
      address: "mail.send-email",
      request: {
        operation: "enqueue",
        payload: { to: "a@example.com" },
        opts: { max_attempts: 5, run_at_ms: 1234, id: "stable-id" },
      },
    },
    {
      address: "web.progress",
      request: {
        operation: "publish",
        params: { jobId: "job-1" },
        message: { type: "update", data: { percent: 50 } },
      },
    },
  ]);
});

test("network handles cannot silently fall back to public HTTP", async () => {
  await expect(workflow("mail.send-email").enqueue({})).rejects.toThrow("server runtime");
  await expect(channel("web.progress").publish({ type: "update", data: {} })).rejects.toThrow(
    "server runtime",
  );
});

test("recipient rejects missing definitions and invalid params and owns channel lifecycle", async () => {
  const registry = new ChannelRegistry();
  const definition = defineChannel("progress", {
    ephemeral: true,
    paramsSchema: (t) => t.Object({ jobId: t.String() }),
  });
  registry.register("progress", definition.definition);
  const calls: unknown[] = [];
  setChannelSocketPublisher(async (name, message) => {
    calls.push({ name, message });
    return { id: "", ...message } as never;
  });
  const invoke = (name: string, params: unknown) =>
    handleNetworkInvocation(
      new Request("http://app.tako/network", {
        method: "POST",
        body: JSON.stringify({
          name,
          request: {
            operation: "publish",
            params,
            message: { type: "update", data: { percent: 20 } },
            lifecycle: { ephemeral: false },
          },
        }),
      }),
      registry,
    );
  expect((await invoke("missing", {})).status).toBe(404);
  expect((await invoke("progress", {})).status).toBe(400);
  expect((await invoke("progress", { jobId: "j1" })).status).toBe(200);
  expect(calls).toHaveLength(1);
  expect(calls[0]).toMatchObject({ name: "progress", message: { lifecycle: { ephemeral: true } } });
});

test("recipient workflow owns retries and acknowledges enqueue without waiting for execution", async () => {
  const { mkdtemp, mkdir, writeFile, rm } = await import("node:fs/promises");
  const { createServer } = await import("node:net");
  const { tmpdir } = await import("node:os");
  const { join, relative, resolve } = await import("node:path");
  const root = await mkdtemp(join(tmpdir(), "tako-network-recipient-"));
  const saved = {
    TAKO_APP_ROOT: process.env.TAKO_APP_ROOT,
    TAKO_APP_NAME: process.env.TAKO_APP_NAME,
    TAKO_INTERNAL_SOCKET: process.env.TAKO_INTERNAL_SOCKET,
  };
  const captured: any[] = [];
  const server = createServer((socket) =>
    socket.once("data", (data) => {
      captured.push(JSON.parse(data.toString()));
      socket.end(
        JSON.stringify({ status: "ok", data: { id: "remote-run", deduplicated: false } }) + "\n",
      );
    }),
  );
  try {
    await mkdir(join(root, "workflows"));
    await writeFile(
      join(root, "workflows/send-email.ts"),
      `import { defineWorkflow } from ${JSON.stringify(resolve(import.meta.dirname, "../../src/workflows/define.ts"))}; export default defineWorkflow("send-email", { retries: 4, handler: async () => { throw new Error("must run in the worker"); } });`,
    );
    process.env.TAKO_APP_ROOT = relative(process.cwd(), root);
    process.env.TAKO_APP_NAME = "mail/staging";
    process.env.TAKO_INTERNAL_SOCKET = join(root, "rpc.sock");
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(process.env.TAKO_INTERNAL_SOCKET, resolve);
    });
    const invoke = (opts: object) =>
      handleNetworkInvocation(
        new Request("http://mail.tako/network", {
          method: "POST",
          body: JSON.stringify({
            name: "send-email",
            request: { operation: "enqueue", payload: { to: "a@example.com" }, opts },
          }),
        }),
        new ChannelRegistry(),
      );
    expect(await (await invoke({ id: "stable" })).json()).toEqual({
      status: "ok",
      data: { id: "remote-run", deduplicated: false },
    });
    expect((await invoke({ max_attempts: 2 })).status).toBe(200);
    expect((await invoke({ max_attempts: 0 })).status).toBe(400);
    expect(captured).toHaveLength(2);
    expect(captured[0]).toMatchObject({
      command: "enqueue_run",
      app: "mail/staging",
      name: "send-email",
      opts: { id: "stable", max_attempts: 5 },
    });
    expect(captured[1].opts).toEqual({ max_attempts: 2 });
  } finally {
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    if (server.listening) await new Promise<void>((resolve) => server.close(() => resolve()));
    await rm(root, { recursive: true, force: true });
  }
});
