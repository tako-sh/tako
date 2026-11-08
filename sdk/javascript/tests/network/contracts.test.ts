import { afterEach, expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import ts from "typescript";
import { generateNetworkTypes } from "../../src/network/generate";
import { discoverChannels } from "../../src/channels/discovery";
import { discoverWorkflows } from "../../src/workflows/discovery";

const temps: string[] = [];
afterEach(async () => {
  for (const dir of temps.splice(0)) await rm(dir, { recursive: true, force: true });
});
async function project() {
  const dir = await mkdtemp(join(tmpdir(), "tako-network-"));
  temps.push(dir);
  for (const folder of ["src/channels", "src/workflows", "node_modules", "shared"])
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
    join(consumer, "src/workflows/network.d.ts"),
    'export type { SendEmailWorkflow } from "../../shared/contracts";',
  );
  await writeFile(
    join(consumer, "src/channels/network.d.ts"),
    'export type { ProgressChannel as JobProgress } from "../../shared/contracts";',
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
    `import { channel, workflow } from "tako.sh";
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
});

test("runtime discovery skips declaration files, including consumer-only directories", async () => {
  const dir = await project();
  for (const folder of ["channels", "workflows"])
    await writeFile(
      join(dir, "src", folder, "network.d.ts"),
      'export type { Remote } from "not-installed";',
    );
  expect(await discoverChannels(join(dir, "src/channels"))).toEqual([]);
  expect(await discoverWorkflows(join(dir, "src/workflows"))).toEqual([]);
  await expect(generateNetworkTypes(join(dir, "src"), join(dir, "src"), "web")).rejects.toThrow();
});

test("rejects wrong primitive kinds and duplicate external addresses", async () => {
  const dir = await project();
  const external = join(dir, "src/workflows/network.d.ts");
  await writeFile(
    external,
    'export type Wrong = { kind: "channel"; address: "mail.chat"; params: {}; messages: {} };',
  );
  await expect(generateNetworkTypes(join(dir, "src"), join(dir, "src"), "web")).rejects.toThrow(
    "workflow contract",
  );
  await writeFile(
    external,
    'export type One = { kind: "workflow"; address: "mail.send"; payload: { to: string } }; export type Two = One;',
  );
  await expect(generateNetworkTypes(join(dir, "src"), join(dir, "src"), "web")).rejects.toThrow(
    "duplicate network address",
  );
});
