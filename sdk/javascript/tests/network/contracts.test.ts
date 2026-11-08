import { afterEach, expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import ts from "typescript";
import { generateNetworkTypes } from "../../src/network/generate";
import { discoverChannels } from "../../src/channels/discovery";
import { discoverWorkflows } from "../../src/workflows/discovery";

const temps: string[] = [];

test("local-only channel names do not block exports and raw channel definitions retain params", async () => {
  const dir = await project();
  await writeFile(
    join(dir, "src/channels/chat.ts"),
    'import { defineChannel } from "tako.sh"; export default defineChannel("chat/room-123");',
  );
  await writeFile(
    join(dir, "src/channels/orders.ts"),
    'import { defineChannel } from "tako.sh"; export default defineChannel("orders.updated");',
  );
  await writeFile(
    join(dir, "src/channels/status.ts"),
    'import { defineChannel } from "tako.sh"; export default defineChannel("status", { paramsSchema: t => t.Object({ jobId: t.String() }) }).definition;',
  );
  const result = await generateNetworkTypes(join(dir, "src"), dir, "mail");
  expect((await discoverChannels(join(dir, "src/channels"))).length).toBe(3);
  expect(result.contracts).not.toContain("chat/room-123");
  expect(result.contracts).not.toContain("orders.updated");
  expect(result.contracts).toContain('"jobId": string');
  expect(result.contracts).toContain("messages: Record<string, unknown>");
  expect(result.schema["x-tako"].primitives).toHaveLength(1);
  expect(result.schema["x-tako"].primitives[0]!.params).toMatchObject({
    type: "object",
    required: ["jobId"],
  });
});
afterEach(async () => {
  for (const dir of temps.splice(0)) await rm(dir, { recursive: true, force: true });
});
async function project() {
  const dir = await mkdtemp(join(tmpdir(), "tako-network-"));
  temps.push(dir);
  for (const folder of [
    "src/channels",
    "src/workflows",
    "src/network-handlers",
    "node_modules",
    "shared",
  ])
    await mkdir(join(dir, folder), { recursive: true });
  await symlink(resolve(import.meta.dirname, "../.."), join(dir, "node_modules/tako.sh"));
  return dir;
}

test("exports standalone contracts and imports only the selected remote primitives", async () => {
  const producer = await project();
  await writeFile(
    join(producer, "src/payload.ts"),
    `export interface Recipient { to: string; next?: Recipient; tags?: readonly string[]; position?: readonly [number, number?] }`,
  );
  await writeFile(
    join(producer, "src/workflows/send-email.ts"),
    `import { defineWorkflow } from "tako.sh"; import type { Recipient } from "../payload"; export default defineWorkflow<Recipient>("send-email", { retries: 4, handler: async () => {} });`,
  );
  await writeFile(
    join(producer, "src/channels/progress.ts"),
    `import { defineChannel } from "tako.sh"; export default defineChannel("progress", { paramsSchema: t => t.Object({ jobId: t.String() }) }).$messageTypes<{ update: { percent: number } }>();`,
  );
  await writeFile(
    join(producer, "src/network-handlers/lookup.ts"),
    "export default async function lookup(input: { id: `user_${string}` }) { return { id: input.id, active: true }; }",
  );
  const generated = await generateNetworkTypes(
    join(producer, "src"),
    join(producer, "src"),
    "notifications",
  );
  expect(generated.contracts).toContain('"notifications.send-email"');
  expect(generated.contracts).toContain("SendEmailWorkflow");
  expect(generated.contracts).not.toContain("import(");
  expect(generated.contracts).not.toContain(producer);

  const consumer = await project();
  await writeFile(join(consumer, "shared/contracts.d.ts"), generated.contracts);
  await rm(producer, { recursive: true, force: true });
  await writeFile(
    join(consumer, "src/tako.imports.ts"),
    'export type { SendEmailWorkflow, ProgressChannel as JobProgress, LookupHandler } from "../shared/contracts";',
  );
  const local = await generateNetworkTypes(join(consumer, "src"), join(consumer, "src"), "web");
  expect(local.declarations).toContain('"notifications.send-email"');
  expect(local.declarations).not.toContain('"send-email":');
  expect(local.contracts).not.toContain("SendEmailWorkflow");
  await writeFile(
    join(consumer, "src/tako.d.ts"),
    `import "tako.sh"; declare module "tako.sh" { ${local.declarations} }`,
  );
  const main = join(consumer, "src/main.ts");
  await writeFile(
    main,
    `import { call, channel, workflow } from "tako.sh";
const response: Promise<{ id: \`user_\${string}\`; active: boolean }> = call("notifications.lookup", { id: "user_123" });
// @ts-expect-error preserves template literal input
call("notifications.lookup", { id: "bad" });
const payload = { to: "a@example.com", next: { to: "b@example.com" }, tags: ["welcome"], position: [1, 2] } as const;
workflow("notifications.send-email").enqueue(payload);
channel("notifications.progress", { jobId: "j1" }).publish({ type: "update", data: { percent: 50 } });
// @ts-expect-error required workflow payload
workflow("notifications.send-email").enqueue({ email: "wrong" });
// @ts-expect-error undeclared address
workflow("other.send-email");
// @ts-expect-error required channel params
channel("notifications.progress");
// @ts-expect-error incorrect message payload
channel("notifications.progress", { jobId: "j1" }).publish({ type: "update", data: { percent: "wrong" } });
`,
  );
  const program = ts.createProgram([main, join(consumer, "src/tako.d.ts")], {
    target: ts.ScriptTarget.ESNext,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    strict: true,
    skipLibCheck: true,
    noEmit: true,
    types: [],
  });
  expect(
    ts
      .getPreEmitDiagnostics(program)
      .map((d) => ts.flattenDiagnosticMessageText(d.messageText, "\n")),
  ).toEqual([]);
}, 30_000);

test("runtime discovery skips declaration files, including consumer-only directories", async () => {
  const dir = await project();
  for (const folder of ["channels", "workflows"])
    await writeFile(
      join(dir, "src", folder, "network.d.ts"),
      'export type { Remote } from "not-installed";',
    );
  expect(await discoverChannels(join(dir, "src/channels"))).toEqual([]);
  expect(await discoverWorkflows(join(dir, "src/workflows"))).toEqual([]);
  expect(
    (await generateNetworkTypes(join(dir, "src"), join(dir, "src"), "web")).contracts,
  ).toContain("export {};");
});

test("rejects wrong primitive kinds and duplicate external addresses", async () => {
  const dir = await project();
  const external = join(dir, "src/tako.imports.ts");
  await writeFile(
    external,
    'export type Wrong = { kind: "invalid"; address: "mail.chat"; params: {}; messages: {} };',
  );
  await expect(generateNetworkTypes(join(dir, "src"), join(dir, "src"), "web")).rejects.toThrow(
    "workflow, channel, or handler contract",
  );
  await writeFile(
    external,
    'export type One = { kind: "workflow"; address: "mail.send"; payload: { to: string }; result: null }; export type Two = One;',
  );
  await expect(generateNetworkTypes(join(dir, "src"), join(dir, "src"), "web")).rejects.toThrow(
    "duplicate network address",
  );
});

test("raw workflow definitions export payloads and results alongside root declarations", async () => {
  const dir = await project();
  await writeFile(
    join(dir, "src/workflows/lookup.ts"),
    'import { defineWorkflow } from "tako.sh"; export default defineWorkflow("lookup", { handler: async (input: { id: `user_${string}` }) => ({ id: input.id, ok: true }) }).definition;',
  );
  const generated = await generateNetworkTypes(join(dir, "src"), dir, "users");
  expect(generated.contracts).toContain('"users.lookup"');
  expect(generated.contracts).toContain("`user_${string}`");
  expect(generated.contracts).toContain('"ok": boolean');
  expect(generated.declarations).toContain('import("./tako.exports").LookupWorkflow');
});

test("first generation follows result types through local and imported network calls", async () => {
  const dir = await project();
  await writeFile(
    join(dir, "tako.imports.ts"),
    'export type Remote = { kind: "workflow"; address: "mail.deliver"; payload: { to: string }; result: { messageId: string } };',
  );
  await writeFile(
    join(dir, "src/network-handlers/lookup.ts"),
    'import { workflow } from "tako.sh"; export default async (input: { to: string }) => workflow("mail.deliver").run(input);',
  );
  await writeFile(
    join(dir, "src/network-handlers/forward.ts"),
    'import { call } from "tako.sh"; export default async (input: { to: string }) => call("lookup", input);',
  );
  const generated = await generateNetworkTypes(join(dir, "src"), dir, "users");
  expect(generated.contracts).toContain('"messageId": string');
  expect(generated.contracts).not.toContain("result: unknown");
  expect(generated.contracts).toContain("ForwardHandler");
});
