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

async function gen(root: string, args: string[] = []) {
  const proc = Bun.spawn([binary, "--ci", "gen", ...args], {
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
  await addSchema(root, "mail", [
    {
      kind: "workflow",
      name: "send-email",
      payload: {
        type: "object",
        properties: { to: { type: "string" } },
        required: ["to"],
        additionalProperties: false,
      },
      result: { type: "null" },
    },
    {
      kind: "channel",
      name: "progress",
      params: { type: "object", additionalProperties: false },
      messages: {
        type: "object",
        properties: {
          update: {
            type: "object",
            properties: { percent: { type: "number" } },
            required: ["percent"],
            additionalProperties: false,
          },
        },
        additionalProperties: false,
      },
    },
  ]);
  const result = await gen(root);
  expect(result.exitCode, result.output).toBe(0);
  const declarations = await readFile(join(root, "tako.d.ts"), "utf8");
  expect(declarations).toContain('"mail.send-email"');
  expect(declarations).toContain('import("./tako.imports")');
  expect(await readFile(join(root, "tako.exports.d.ts"), "utf8")).toContain("export {};");
  for (const kind of ["channels", "workflows"])
    expect(await stat(join(app, kind, "demo.ts")).catch(() => null)).toBeNull();
  expect(await readFile(join(root, "package.json"), "utf8")).toBe(packageBefore);
  const modified = (await stat(join(root, "tako.imports.ts"))).mtimeMs;
  const repeated = await gen(root);
  expect(repeated.exitCode, repeated.output).toBe(0);
  expect((await stat(join(root, "tako.imports.ts"))).mtimeMs).toBe(modified);

  const config = join(root, "tako.toml");
  await writeFile(config, (await readFile(config, "utf8")).replace(/schemas = .*\n/, ""));
  expect((await gen(root)).exitCode).toBe(0);
  expect(await readFile(join(root, "tako.d.ts"), "utf8")).not.toContain('"mail.send-email"');
  const before = await readFile(join(root, "tako.d.ts"), "utf8");
  await writeFile(config, (await readFile(config, "utf8")) + 'schemas = ["missing.json"]\n');
  expect((await gen(root)).exitCode).not.toBe(0);
  expect(await readFile(join(root, "tako.d.ts"), "utf8")).toBe(before);
}, 60_000);

test("gen keeps imports and portable exports at root when the app uses src", async () => {
  const { root, app } = await project("src");
  await addSchema(root, "users", [
    {
      kind: "handler",
      name: "lookup",
      input: {
        type: "object",
        properties: { id: { type: "string" } },
        required: ["id"],
        additionalProperties: false,
      },
      result: {
        type: "object",
        properties: { name: { type: "string" } },
        required: ["name"],
        additionalProperties: false,
      },
    },
  ]);
  const result = await gen(root);
  expect(result.exitCode, result.output).toBe(0);
  expect(await readFile(join(root, "tako.d.ts"), "utf8")).toContain('"users.lookup"');
  expect(await readFile(join(root, "tako.exports.d.ts"), "utf8")).toContain("export {};");
  expect(await stat(join(app, "tako.d.ts")).catch(() => null)).toBeNull();
}, 30_000);

async function addSchema(root: string, app: string, primitives: unknown[]) {
  await writeFile(
    join(root, "remote-schema.json"),
    JSON.stringify({
      $schema: "https://json-schema.org/draft/2020-12/schema",
      $comment: "Generated by tako gen. Do not edit.",
      $defs: {},
      "x-tako": { version: 1, app, primitives },
    }),
  );
  const config = join(root, "tako.toml");
  await writeFile(config, (await readFile(config, "utf8")) + 'schemas = ["remote-schema.json"]\n');
}

test("remote schemas are pinned, explicitly refreshed, and available offline", async () => {
  const { root } = await project("src");
  let revision = "before";
  let requests = 0;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch() {
      requests++;
      return Response.json({
        $schema: "https://json-schema.org/draft/2020-12/schema",
        $defs: {},
        "x-tako": {
          version: 1,
          app: "users",
          primitives: [
            {
              kind: "handler",
              name: "lookup",
              input: { type: "string" },
              result: { const: revision },
            },
          ],
        },
      });
    },
  });
  try {
    const config = join(root, "tako.toml");
    await writeFile(
      config,
      (await readFile(config, "utf8")) +
        `schemas = ["http://127.0.0.1:${server.port}/schema.json"]\n`,
    );
    let result = await gen(root);
    expect(result.exitCode, result.output).toBe(0);
    const firstLock = await readFile(join(root, "tako-schema.lock"), "utf8");
    revision = "after";
    result = await gen(root);
    expect(result.exitCode, result.output).toBe(0);
    expect(requests).toBe(1);
    expect(await readFile(join(root, "tako.imports.ts"), "utf8")).toContain('"before"');
    result = await gen(root, ["--update-schemas"]);
    expect(result.exitCode, result.output).toBe(0);
    expect(requests).toBe(2);
    expect(await readFile(join(root, "tako-schema.lock"), "utf8")).not.toBe(firstLock);
    expect(await readFile(join(root, "tako.imports.ts"), "utf8")).toContain('"after"');
    server.stop(true);
    await rm(join(root, "tako.imports.ts"));
    result = await gen(root);
    expect(result.exitCode, result.output).toBe(0);
    expect(await readFile(join(root, "tako.imports.ts"), "utf8")).toContain('"after"');
    const lock = join(root, "tako-schema.lock");
    await writeFile(lock, (await readFile(lock, "utf8")).replace('"after"', '"tampered"'));
    result = await gen(root);
    expect(result.exitCode).not.toBe(0);
    expect(result.output).toContain("hash mismatch");
  } finally {
    server.stop(true);
  }
}, 90_000);
