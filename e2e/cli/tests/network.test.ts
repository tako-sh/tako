import { afterEach, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, writeFile, rm, symlink, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const repo = resolve(import.meta.dirname, "../../..");
const binary = process.env.TAKO_BIN ?? join(repo, "target/debug/tako");
const temps: string[] = [];
afterEach(async () => {
  for (const path of temps.splice(0)) await rm(path, { recursive: true, force: true });
});

async function project(appRoot = "packages/api/src") {
  const root = await mkdtemp(join(tmpdir(), "tako-gen-network-"));
  temps.push(root);
  const app = join(root, appRoot);
  await mkdir(app, { recursive: true });
  await mkdir(join(root, "node_modules/.bin"), { recursive: true });
  await symlink(join(repo, "sdk/javascript"), join(root, "node_modules/tako.sh"));
  for (const kind of ["channel", "network", "store"])
    await symlink(
      join(repo, `sdk/javascript/dist/gen-${kind}-types.mjs`),
      join(root, `node_modules/.bin/tako-sh-gen-${kind}-types`),
    );
  await writeFile(join(root, "package.json"), '{"type":"module","dependencies":{"tako.sh":"*"}}');
  await writeFile(
    join(root, "tako.toml"),
    `name = "web"\nruntime = "bun"\napp_root = "${appRoot}"\n`,
  );
  return { root, app };
}

async function gen(root: string) {
  const proc = Bun.spawn([binary, "--ci", "gen"], {
    cwd: root,
    env: { ...process.env, TAKO_HOME: join(root, ".tako-home") },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { output: stdout + stderr, exitCode };
}

test("gen uses consumer network declarations without scaffolding handlers or changing package exports", async () => {
  const { root, app } = await project();
  const packageBefore = await readFile(join(root, "package.json"), "utf8");
  await writeFile(
    join(root, "contracts.d.ts"),
    'export type SendEmailWorkflow = { kind: "workflow"; address: "mail.send-email"; payload: { to: string }; result: null }; export type ProgressChannel = { kind: "channel"; address: "mail.progress"; params: {}; messages: { update: { percent: number } } };',
  );
  await writeFile(
    join(root, "tako.imports.ts"),
    'export type { SendEmailWorkflow, ProgressChannel } from "./contracts";',
  );
  const result = await gen(root);
  expect(result.exitCode, result.output).toBe(0);
  const declarations = await readFile(join(root, "tako.d.ts"), "utf8");
  expect(declarations).toContain('"mail.send-email"');
  expect(declarations).toContain('import("./tako.imports")');
  expect(await readFile(join(root, "tako.exports.d.ts"), "utf8")).toContain("export {};");
  for (const kind of ["channels", "workflows"])
    expect(await stat(join(app, kind, "demo.ts")).catch(() => null)).toBeNull();
  expect(await readFile(join(root, "package.json"), "utf8")).toBe(packageBefore);
  expect((await gen(root)).output).toContain("up to date");

  await writeFile(join(root, "tako.imports.ts"), "export {};");
  expect((await gen(root)).exitCode).toBe(0);
  expect(await readFile(join(root, "tako.d.ts"), "utf8")).not.toContain('"mail.send-email"');
  const before = await readFile(join(root, "tako.d.ts"), "utf8");
  await writeFile(join(root, "tako.imports.ts"), 'export type { Missing } from "missing-package";');
  expect((await gen(root)).exitCode).not.toBe(0);
  expect(await readFile(join(root, "tako.d.ts"), "utf8")).toBe(before);
}, 60_000);

test("gen keeps imports and portable exports at root when the app uses src", async () => {
  const { root, app } = await project("src");
  await writeFile(
    join(root, "contracts.d.ts"),
    'export type LookupHandler = { kind: "handler"; address: "users.lookup"; input: { id: string }; result: { name: string } };',
  );
  await writeFile(
    join(root, "tako.imports.ts"),
    'export type { LookupHandler } from "./contracts";',
  );
  const result = await gen(root);
  expect(result.exitCode, result.output).toBe(0);
  expect(await readFile(join(root, "tako.d.ts"), "utf8")).toContain('"users.lookup"');
  expect(await readFile(join(root, "tako.exports.d.ts"), "utf8")).toContain("export {};");
  expect(await stat(join(app, "tako.d.ts")).catch(() => null)).toBeNull();
}, 30_000);
