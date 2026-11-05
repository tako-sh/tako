import { expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { Type as t } from "@sinclair/typebox";
import { defineService } from "../../src/services/contract";
import { createServiceHandler } from "../../src/services/handler";
import { generateServiceDocument } from "../../src/services/generate";
import { generateServiceTypes } from "../../bin/gen-service-types";

const sdkRoot = resolve(import.meta.dir, "../..");
async function run(command: string[], cwd = sdkRoot) {
  const logs = await mkdtemp(join(tmpdir(), "tako-service-command-"));
  try {
    const stdoutFile = Bun.file(join(logs, "stdout"));
    const stderrFile = Bun.file(join(logs, "stderr"));
    const child = Bun.spawn(command, { cwd, stdout: stdoutFile, stderr: stderrFile });
    const code = await child.exited;
    const [stdout, stderr] = await Promise.all([stdoutFile.text(), stderrFile.text()]);
    if (code !== 0) throw new Error(`${command.join(" ")} (${code})\n${stdout}${stderr}`);
    return stdout;
  } finally {
    await rm(logs, { recursive: true, force: true });
  }
}
const compiler = [
  process.execPath,
  join(sdkRoot, "node_modules/typescript/bin/tsc"),
  "--ignoreConfig",
  "--strict",
  "--skipLibCheck",
  "--target",
  "ESNext",
  "--module",
  "ESNext",
  "--moduleResolution",
  "bundler",
  "--lib",
  "ESNext,DOM",
];

test("generation is deterministic, escapes names, and retains schema constraints", () => {
  const first = defineService('service"雪', {
    ["__proto__"]: {
      method: "POST",
      path: "/items",
      body: t.Object(
        { ["__proto__"]: t.String(), title: t.Optional(t.String({ minLength: 1 })) },
        { additionalProperties: false },
      ),
      responses: { 201: t.Union([t.Literal("created"), t.Literal("exists")]) },
    },
  });
  const other = defineService("other", {
    get: { method: "GET", path: "/", responses: { 200: t.Null() } },
  });
  const output = generateServiceDocument([first, other]);
  expect(output).toBe(generateServiceDocument([other, first]));
  expect(output).toContain('["__proto__"]');
  expect(output).toContain('t.Optional(t.String({ ["minLength"]: 1 }))');
  expect(output).not.toContain("typeof import");
  expect(JSON.parse(generateServiceDocument([first], "manifest"))).toMatchObject({
    services: [{ name: 'service"雪' }],
  });
  expect(JSON.parse(generateServiceDocument([first], "openapi"))).toMatchObject({
    openapi: "3.1.0",
  });
  expect(() => generateServiceDocument([first, first])).toThrow("Duplicate");
});

test("generated package compiles and runs after deleting the producer app", async () => {
  const root = await mkdtemp(join(tmpdir(), "tako-service-consumer-"));
  const producer = join(root, "producer");
  const servicesDir = join(producer, "services");
  const consumer = join(root, "consumer");
  const packageDir = join(root, "package");
  try {
    await mkdir(servicesDir, { recursive: true });
    await mkdir(join(consumer, "node_modules"), { recursive: true });
    await mkdir(packageDir);
    await symlink(join(sdkRoot, "node_modules"), join(producer, "node_modules"), "dir");
    await symlink(join(sdkRoot, "node_modules"), join(packageDir, "node_modules"), "dir");
    await symlink(packageDir, join(consumer, "node_modules/tako.sh"), "dir");
    await mkdir(join(consumer, "node_modules/@sinclair"));
    await symlink(
      join(sdkRoot, "node_modules/@sinclair/typebox"),
      join(consumer, "node_modules/@sinclair/typebox"),
      "dir",
    );
    await writeFile(
      join(packageDir, "package.json"),
      JSON.stringify({
        name: "tako.sh",
        type: "module",
        exports: { "./services": { types: "./dist/index.d.ts", import: "./dist/services.js" } },
      }),
    );
    const build = await Bun.build({
      entrypoints: [join(sdkRoot, "src/services/index.ts")],
      outdir: join(packageDir, "dist"),
      naming: "services.js",
      target: "browser",
      external: ["@sinclair/typebox", "@sinclair/typebox/value"],
    });
    expect(build.success).toBe(true);
    await run([
      ...compiler,
      "--declaration",
      "--emitDeclarationOnly",
      "--outDir",
      join(packageDir, "dist"),
      join(sdkRoot, "src/services/index.ts"),
    ]);
    await writeFile(
      join(servicesDir, "catalog.ts"),
      `
import { Type as t } from "@sinclair/typebox";
import { defineService } from ${JSON.stringify(pathToFileURL(join(sdkRoot, "src/services/contract.ts")).href)};
export default defineService("catalog", {
  get: { method: "GET", path: "/items/{id}", params: t.Object({ id: t.String() }), query: t.Object({ limit: t.Optional(t.Integer()) }), responses: { 200: t.Object({ title: t.String() }), 404: t.Object({ error: t.String() }) } },
  create: { method: "POST", path: "/items", body: t.Object({ title: t.String({ minLength: 1 }), tags: t.Optional(t.Array(t.String())) }, { required: ["title"] }), responses: { 201: t.Object({ title: t.String() }) } },
  remove: { method: "DELETE", path: "/items", responses: { 204: null } },
});`,
    );
    await writeFile(join(servicesDir, "ignored.d.ts"), "declare const anything: never;");
    const generated = await generateServiceTypes(servicesDir);
    const cli = await run([
      process.execPath,
      join(sdkRoot, "bin/gen-service-types.ts"),
      servicesDir,
    ]);
    expect(cli).toBe(generated);
    const manifest = await run([
      process.execPath,
      join(sdkRoot, "bin/gen-service-types.ts"),
      servicesDir,
      "--format",
      "manifest",
    ]);
    expect(JSON.parse(manifest)).toMatchObject({ services: [{ name: "catalog" }] });
    expect(generated).not.toContain(producer);
    await writeFile(join(consumer, "contracts.ts"), generated);
    await rm(producer, { recursive: true, force: true });
    await writeFile(
      join(consumer, "types.ts"),
      `
import { createClient, services } from "./contracts";
import { createServiceHandler, type ServiceResponse } from "tako.sh/services";
const client = createClient("catalog", { baseURL: "http://ordinary-app" });
async function check() {
  const response = await client.get({ params: { id: "a" }, query: { limit: 2 } });
  if (response.status === 200) { const title: string = response.body.title; void title; }
  if (response.status === 404) { const error: string = response.body.error; void error; }
  const empty: undefined = (await client.remove()).body;
  void empty;
  await client.create({ body: { title: "valid", tags: ["x"] } });
  // @ts-expect-error Param type must survive generation.
  await client.get({ params: { id: 1 } });
  // @ts-expect-error Required params cannot be omitted.
  await client.get();
  // @ts-expect-error Query types must survive generation.
  await client.get({ params: { id: "a" }, query: { limit: "2" } });
  // @ts-expect-error Request body must survive generation.
  await client.create({ body: { title: 1 } });
  // @ts-expect-error Body cannot be omitted.
  await client.create();
  // @ts-expect-error Explicit consistent required metadata preserves static required fields.
  await client.create({ body: { tags: [] } });
  // @ts-expect-error Unknown operation.
  await client.missing();
  // @ts-expect-error Status must narrow before body fields are used.
  response.body.title;
  // @ts-expect-error Unknown service.
  createClient("missing", { baseURL: "http://ordinary-app" });
}
void check;
const expected: ServiceResponse<typeof services.catalog.operations.get> = { status: 404, body: { error: "missing" } };
void expected;
// @ts-expect-error Status and payload must agree.
const wrong: ServiceResponse<typeof services.catalog.operations.get> = { status: 404, body: { title: "wrong" } };
void wrong;
createServiceHandler(services.catalog, {
  get: ({ params }) => params.id ? { status: 200, body: { title: params.id } } : { status: 404, body: { error: "missing" } },
  create: async ({ body }) => ({ status: 201, body: { title: body.title } }),
  remove: () => ({ status: 204, body: undefined }),
});
`,
    );
    await run([...compiler, "--noEmit", join(consumer, "types.ts")]);
    await writeFile(join(consumer, "empty.ts"), generateServiceDocument([]));
    await writeFile(
      join(consumer, "empty-response.ts"),
      generateServiceDocument([
        defineService("empty", { get: { method: "GET", path: "/", responses: { 204: null } } }),
      ]),
    );
    await run([
      ...compiler,
      "--noEmit",
      "--noUnusedLocals",
      join(consumer, "empty.ts"),
      join(consumer, "empty-response.ts"),
    ]);

    await writeFile(
      join(consumer, "runtime.ts"),
      `
import { createClient } from "./contracts";
const client = createClient("catalog", { baseURL: "http://ordinary-app", fetch: async (input, init) => {
  if (new URL(String(input)).pathname !== "/items" || init?.method !== "POST") throw new Error("Not standard HTTP");
  return Response.json(JSON.parse(String(init.body)), { status: 201 });
} });
const result = await client.create({ body: { title: "shared contract" } });
if (result.body.title !== "shared contract") throw new Error("Wrong response");
try { await client.create({ body: { title: "" } }); throw new Error("Validation lost"); }
catch (error) { if (!(error instanceof Error) || !error.message.includes("Invalid body")) throw error; }
console.log("consumer passed");
`,
    );
    expect(await run([process.execPath, join(consumer, "runtime.ts")], consumer)).toBe(
      "consumer passed\n",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}, 60_000);

test("accepted explicit required metadata preserves generated runtime behavior", async () => {
  const root = await mkdtemp(join(tmpdir(), "tako-service-required-"));
  try {
    await symlink(join(sdkRoot, "node_modules"), join(root, "node_modules"), "dir");
    const original = defineService("required", {
      post: {
        method: "POST",
        path: "/",
        body: t.Object(
          {
            token: t.String(),
            note: t.Optional(t.String()),
          },
          { required: ["token"], additionalProperties: false },
        ),
        responses: { 204: null },
      },
    });
    const generated = generateServiceDocument([original]).replace(
      '"tako.sh/services"',
      JSON.stringify(pathToFileURL(join(sdkRoot, "src/services/index.ts")).href),
    );
    const file = join(root, "contract.ts");
    await writeFile(file, generated);
    const { services } = (await import(pathToFileURL(file).href)) as {
      services: { required: typeof original };
    };
    for (const service of [original, services.required]) {
      const handler = createServiceHandler(service, {
        post: () => ({ status: 204, body: undefined }),
      });
      for (const [body, status] of [
        [{}, 400],
        [{ token: "ok" }, 204],
        [{ token: "ok", note: "optional" }, 204],
      ] as const)
        expect(
          (
            await handler(
              new Request("http://fixture/", {
                method: "POST",
                headers: { "content-type": "application/json" },
                body: JSON.stringify(body),
              }),
            )
          ).status,
        ).toBe(status);
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("discovery rejects duplicate names and malformed exports; a missing directory is empty", async () => {
  const root = await mkdtemp(join(tmpdir(), "tako-service-discovery-"));
  try {
    expect(await generateServiceTypes(join(root, "missing"), "manifest")).toBe(
      '{\n  "services": []\n}\n',
    );
    await writeFile(join(root, "a.ts"), 'export default { name: "same", operations: {} };');
    await writeFile(join(root, "b.ts"), 'export default { name: "same", operations: {} };');
    await expect(generateServiceTypes(root)).rejects.toThrow("Duplicate service");
    await rm(join(root, "b.ts"));
    await writeFile(join(root, "invalid.ts"), "export default () => {};");
    await expect(generateServiceTypes(root)).rejects.toThrow("default-export");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
