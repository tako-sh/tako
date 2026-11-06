import { afterAll, afterEach, beforeAll, beforeEach, expect, test } from "bun:test";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  stat,
  symlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

const repo = resolve(import.meta.dirname, "../../..");
const sdk = join(repo, "sdk/javascript");
const tako = process.env["TAKO_BIN"] ?? join(repo, "target/debug/tako");
let suite: string;
let fixture: string;

function run(command: string[], cwd: string) {
  const result = Bun.spawnSync(command, {
    cwd,
    env: {
      ...process.env,
      RUST_LOG: "tako=trace,rustls=off,warn",
      TAKO_HOME: join(fixture ?? suite, ".tako-home"),
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  return {
    exitCode: result.exitCode,
    stdout: result.stdout.toString(),
    stderr: result.stderr.toString(),
  };
}

function checked(command: string[], cwd: string) {
  const result = run(command, cwd);
  expect(result.exitCode, result.stdout + result.stderr).toBe(0);
  return result;
}

beforeAll(async () => {
  suite = await mkdtemp(join(tmpdir(), "tako-service-cli-"));
  checked([process.execPath, "run", "build"], sdk);
  const archive = join(suite, "sdk.tgz");
  checked([process.execPath, "pm", "pack", "--ignore-scripts", "--filename", archive], sdk);
  const modules = join(suite, "node_modules");
  const packed = join(modules, "tako.sh");
  await mkdir(packed, { recursive: true });
  checked(["tar", "-xzf", archive, "--strip-components=1", "-C", packed], suite);
  for (const name of ["@sinclair/typebox"]) {
    const destination = join(modules, name);
    await mkdir(dirname(destination), { recursive: true });
    await symlink(await realpath(join(sdk, "node_modules", name)), destination, "dir");
  }
  const bin = join(modules, ".bin");
  await mkdir(bin);
  await symlink("../tako.sh/dist/gen-service-types.mjs", join(bin, "tako-sh-gen-service-types"));
}, 120_000);

beforeEach(async () => {
  fixture = await mkdtemp(join(suite, "fixture-"));
});

afterEach(async () => {
  if (fixture) await rm(fixture, { recursive: true, force: true });
});

afterAll(async () => {
  if (suite) await rm(suite, { recursive: true, force: true });
});

const declaration = `import { Type as t } from "@sinclair/typebox";
import { defineService } from "tako.sh/services";
export default defineService("catalog", {
  get: {
    method: "GET", path: "/items/{id}",
    params: t.Object({ id: t.String() }),
    responses: {
      200: t.Object({ title: t.String() }),
      404: t.Object({ error: t.String() }),
    },
  },
});
`;

async function project(runtime = "bun", appRoot = "src") {
  const producer = join(fixture, "producer app");
  const root = join(producer, appRoot);
  await mkdir(join(root, "services"), { recursive: true });
  await writeFile(
    join(producer, "package.json"),
    JSON.stringify({ name: "service-fixture", type: "module" }),
  );
  await writeFile(
    join(producer, "tako.toml"),
    `name = "service-fixture"\nruntime = "${runtime}"\napp_root = "${appRoot}"\n`,
  );
  await writeFile(join(root, "services/catalog.ts"), declaration);
  await writeFile(join(producer, "tako.d.ts"), "// Previous declarations\n");
  return { producer, root, output: join(root, "tako.services.ts") };
}

for (const [runtime, appRoot] of [
  ["bun", "src"],
  ["node", "client/app"],
  ["node", "."],
] as const) {
  test(`generate uses the installed SDK for ${runtime} with app_root=${appRoot}`, async () => {
    const { producer, root, output } = await project(runtime, appRoot);
    const generated = checked([tako, "--ci", "generate"], producer);
    expect(generated.stdout).toBe("");
    expect(generated.stderr).toContain("tako.services.ts");
    const content = await readFile(output, "utf8");
    expect(content).toContain('defineService("catalog"');
    expect(content).toContain('from "tako.sh/services"');
    expect(content).toContain("export type ServiceContracts");
    expect(content).not.toContain("declare module");
    expect(content).not.toContain(producer);
    expect(content).not.toContain("./services/catalog");
    expect(await readFile(join(producer, "tako.d.ts"), "utf8")).not.toContain("defineService");
    const oldTime = new Date("2000-01-01T00:00:00Z");
    await utimes(output, oldTime, oldTime);
    const unchanged = checked([tako, "--ci", "generate"], producer);
    expect(unchanged.stderr).toContain("up to date");
    expect((await stat(output)).mtimeMs).toBe(oldTime.getTime());

    await rm(join(root, "services/catalog.ts"));
    checked([tako, "--ci", "generate"], producer);
    const empty = await readFile(output, "utf8");
    expect(empty).not.toContain('"catalog"');
    expect(empty).toContain("export const services = {\n};");
    await writeFile(join(root, "services/catalog.mts"), declaration);
    checked([tako, "--ci", "generate"], producer);
    await rm(join(root, "services"), { recursive: true });
    checked([tako, "--ci", "generate"], producer);
    expect(await readFile(output, "utf8")).toBe(empty);
  });
}

test("generation errors preserve both files and report SDK diagnostics", async () => {
  const { producer, root, output } = await project();
  checked([tako, "--ci", "generate"], producer);
  const previous = await readFile(output, "utf8");
  const previousDeclarations = await readFile(join(producer, "tako.d.ts"), "utf8");
  await writeFile(join(root, "services/duplicate.ts"), declaration);
  const failed = run([tako, "--ci", "generate"], producer);
  expect(failed.exitCode).not.toBe(0);
  expect(failed.stdout).toBe("");
  expect(failed.stderr).toContain("service generation failed");
  expect(failed.stderr).toContain("Duplicate service 'catalog'");
  expect(await readFile(output, "utf8")).toBe(previous);
  expect(await readFile(join(producer, "tako.d.ts"), "utf8")).toBe(previousDeclarations);
});

test("Node projects without services generate existing types without Bun on PATH", async () => {
  const { producer, root, output } = await project("node");
  await rm(join(root, "services"), { recursive: true });
  const result = Bun.spawnSync([tako, "--ci", "generate"], {
    cwd: producer,
    env: { ...process.env, PATH: "", TAKO_HOME: join(fixture, ".tako-home") },
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(result.exitCode, result.stderr.toString()).toBe(0);
  expect(await Bun.file(output).exists()).toBe(false);
  expect(await readFile(join(producer, "tako.d.ts"), "utf8")).toContain('declare module "tako.sh"');
});

test("generated contracts typecheck and run as a package after deleting the producer", async () => {
  const { producer, output } = await project();
  checked([tako, "--ci", "generate"], producer);
  const contractPackage = join(fixture, "node_modules/@fixture/catalog");
  await mkdir(contractPackage, { recursive: true });
  await writeFile(
    join(contractPackage, "package.json"),
    JSON.stringify({ name: "@fixture/catalog", type: "module", exports: "./index.ts" }),
  );
  await copyFile(output, join(contractPackage, "index.ts"));
  await rm(producer, { recursive: true });
  await writeFile(
    join(fixture, "tsconfig.json"),
    JSON.stringify({
      compilerOptions: {
        strict: true,
        noEmit: true,
        skipLibCheck: true,
        target: "ESNext",
        module: "ESNext",
        moduleResolution: "bundler",
        types: [],
      },
    }),
  );
  await writeFile(
    join(fixture, "consumer.ts"),
    `import { createClient, type ServiceContracts } from "@fixture/catalog";
const name: keyof ServiceContracts = "catalog";
const client = createClient(name, { baseURL: "https://example.test" });
const result = await client.get({ params: { id: "book" } });
if (result.status === 200) {
  const title: string = result.body.title;
  // @ts-expect-error The success response has no error field.
  result.body.error;
} else {
  const error: string = result.body.error;
}
// @ts-expect-error Path params are required.
client.get({});
// @ts-expect-error Only declared services are available.
createClient("missing", { baseURL: "https://example.test" });
`,
  );
  checked([process.execPath, join(sdk, "node_modules/typescript/bin/tsc"), "-p", fixture], fixture);
  await writeFile(
    join(fixture, "client.ts"),
    `import { createClient } from "@fixture/catalog";
const client = createClient("catalog", {
  baseURL: "https://ordinary-http.example/api",
  fetch: async (input, init) => {
    const request = new Request(input, init);
    if (request.url !== "https://ordinary-http.example/api/items/book%2F1" || request.method !== "GET") throw new Error("unexpected request");
    return Response.json({ title: "Portable contract" });
  },
});
const result = await client.get({ params: { id: "book/1" } });
if (result.status !== 200 || result.body.title !== "Portable contract") throw new Error("unexpected response");
console.log(result.body.title);
`,
  );
  expect(checked([process.execPath, "client.ts"], fixture).stdout.trim()).toBe("Portable contract");
});
