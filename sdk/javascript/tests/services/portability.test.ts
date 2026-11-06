import { expect, test } from "bun:test";
import { mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

test("browser service entrypoint works with string code generation disabled", async () => {
  const root = await mkdtemp(join(tmpdir(), "tako-service-csp-"));
  const sdkRoot = resolve(import.meta.dir, "../..");
  try {
    await symlink(join(sdkRoot, "node_modules"), join(root, "node_modules"), "dir");
    const build = await Bun.build({
      entrypoints: [join(sdkRoot, "src/services/index.ts")],
      outdir: root,
      naming: "services.mjs",
      target: "browser",
      external: ["@sinclair/typebox", "@sinclair/typebox/value"],
    });
    expect(build.success).toBe(true);
    await writeFile(
      join(root, "proof.mjs"),
      `
import assert from "node:assert/strict";
import { Type as t } from "@sinclair/typebox";
import { createServiceClient, defineService } from "./services.mjs";
let calls = 0;
const body = t.Object({ text: t.String({ maxLength: 1, pattern: "^.$" }), number: t.Number({ multipleOf: 0.1 }), count: t.Integer() });
const service = defineService("csp", { post: { method: "POST", path: "/", body, responses: { 200: t.String() } } });
const client = createServiceClient(service, { baseURL: "https://fixture", fetch: async () => { calls++; return Response.json("ok"); } });
assert.equal((await client.post({ body: { text: "😀", number: 0.3, count: 1 } })).body, "ok");
assert.equal(calls, 1);
await assert.rejects(client.post({ body: { text: "xx", number: 0.3, count: 1 } }), { code: "validation" });
assert.equal(calls, 1);
`,
    );
    const child = Bun.spawn(
      ["node", "--disallow-code-generation-from-strings", join(root, "proof.mjs")],
      {
        cwd: root,
        stdout: "pipe",
        stderr: "pipe",
      },
    );
    const [code, stderr] = await Promise.all([child.exited, new Response(child.stderr).text()]);
    expect(stderr).toBe("");
    expect(code).toBe(0);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
