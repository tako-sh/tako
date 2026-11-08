import { expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, writeFile, symlink, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { generateNetworkTypes } from "../../../sdk/javascript/src/network/generate";
import ts from "typescript";

const repo = resolve(import.meta.dirname, "../../..");
const binary = process.env.TAKO_BIN ?? join(repo, "target/debug/tako");
const go = Bun.which("go");
const cargo = Bun.which("cargo");

test.skipIf(!go || !cargo)(
  "schemas compile and round-trip between JavaScript, Go, and Rust",
  async () => {
    const root = await mkdtemp(join(tmpdir(), "tako-schema-native-"));
    const env = {
      ...process.env,
      GOCACHE: process.env.GOCACHE ?? join(repo, "target/schema-go-cache"),
      CARGO_TARGET_DIR: join(repo, "target/schema-codegen-e2e"),
      CARGO_PROFILE_DEV_DEBUG: "0",
      TAKO_HOME: join(root, ".tako-home"),
    };
    const run = async (cwd: string, args: string[]) => {
      const child = Bun.spawn(args, { cwd, env, stdout: "pipe", stderr: "pipe" });
      const [stdout, stderr, code] = await Promise.all([
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
        child.exited,
      ]);
      expect(code, `${args.join(" ")}\n${stdout}\n${stderr}`).toBe(0);
      return stdout;
    };
    try {
      const producer = join(root, "mail");
      const goApp = join(root, "go-app");
      const rustApp = join(root, "rust-app");
      for (const dir of [
        "mail/network-handlers",
        "mail/node_modules",
        "go-app/tako-schema",
        "rust-app/src",
        "rust-app/examples",
      ])
        await mkdir(join(root, dir), { recursive: true });
      await symlink(join(repo, "sdk/javascript"), join(producer, "node_modules/tako.sh"));
      await writeFile(
        join(producer, "network-handlers/lookup.ts"),
        `interface Input { id: string; code: 0 | 1; note?: string | null; next?: Input; position: readonly [number, string]; maybe: string | null; state: { kind: "sent"; messageId: string } | { kind: "missing"; retry?: boolean } } export default (input: Input): Input => input;`,
      );
      const generated = await generateNetworkTypes(producer, producer, "mail");
      await writeFile(join(producer, "tako-schema.json"), JSON.stringify(generated.schema));
      await writeFile(
        join(goApp, "tako.toml"),
        'name = "native-go"\nruntime = "go"\nschemas = ["../mail/tako-schema.json"]\n',
      );
      await writeFile(
        join(goApp, "go.mod"),
        `module example.test/client\n\ngo 1.21\nrequire tako.sh v0.0.0\nreplace tako.sh => ${repo}\n`,
      );
      await writeFile(
        join(goApp, "dto.go"),
        'package client\ntype Query struct { ID uint64 `json:"id,string"`; Note *string `json:"note,omitempty"` }\n',
      );
      await writeFile(
        join(goApp, "tako-schema/main.go"),
        'package main\nimport ("os"; "example.test/client"; "tako.sh/schema")\nfunc main() { app := schema.New("native-go"); schema.Handler[client.Query,client.Query](app,"query"); if err := app.Write(os.Stdout); err != nil { panic(err) } }\n',
      );
      const sample =
        '{"id":"u1","code":1,"note":null,"position":[1.5,"x"],"maybe":null,"state":{"kind":"sent","messageId":"m1"}}';
      await writeFile(
        join(goApp, "roundtrip_test.go"),
        `package client
import ("encoding/json"; "reflect"; "testing")
func TestRoundtrip(t *testing.T) {
 for _, raw := range []string{${JSON.stringify(sample)}, ${JSON.stringify(sample.replace(',"note":null', "").replace('"sent","messageId":"m1"', '"missing","retry":true'))}} {
  var value MailLookupHandlerInput
  if err := json.Unmarshal([]byte(raw), &value); err != nil { t.Fatal(err) }
  output, err := json.Marshal(value); if err != nil { t.Fatal(err) }
  var before, after any; json.Unmarshal([]byte(raw), &before); json.Unmarshal(output, &after)
  if !reflect.DeepEqual(before,after) { t.Fatalf("%s != %s", raw, output) }
 }
 var invalid MailLookupHandlerInput
 if json.Unmarshal([]byte("{}"), &invalid) == nil { t.Fatal("required properties accepted") }
}
`,
      );
      await run(goApp, [binary, "--ci", "gen"]);
      await run(goApp, [go!, "test", "./..."]);
      await writeFile(
        join(rustApp, "tako.toml"),
        'name = "native-rust"\nschemas = ["../mail/tako-schema.json", "../go-app/tako-schema.json"]\n',
      );
      await writeFile(
        join(rustApp, "Cargo.toml"),
        `[package]\nname = "schema-consumer"\nversion = "0.0.0"\nedition = "2024"\n[dependencies]\ntako = { path = ${JSON.stringify(join(repo, "sdk/rust"))}, features = ["schema"] }\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\nschemars = "1.2"\n`,
      );
      await writeFile(
        join(rustApp, "src/lib.rs"),
        `#[path = "../tako_imports.rs"] pub mod imports;
#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)] pub struct Query { pub id: String, #[serde(default, skip_serializing_if="Option::is_none")] pub note: Option<String> }
#[test] fn roundtrip() {
 for raw in [${JSON.stringify(sample)}, ${JSON.stringify(sample.replace(',"note":null', ""))}] {
  let value: imports::MailLookupHandlerInput = serde_json::from_str(raw).unwrap();
  assert_eq!(serde_json::to_value(&value).unwrap(), serde_json::from_str::<serde_json::Value>(raw).unwrap());
 }
 assert!(serde_json::from_str::<imports::MailLookupHandlerInput>("{}").is_err());
 for spelling in ["1.0", "1e0"] {
  let raw = ${JSON.stringify(sample)}.replace("\\\"code\\\":1", &format!("\\\"code\\\":{spelling}"));
  let value: imports::MailLookupHandlerInput = serde_json::from_str(&raw).unwrap();
  assert_eq!(serde_json::to_value(value).unwrap()["code"], 1);
 }
 let invalid = ${JSON.stringify(sample)}.replace("\\\"code\\\":1", "\\\"code\\\":1.5");
 assert!(serde_json::from_str::<imports::MailLookupHandlerInput>(&invalid).is_err());
 let go: imports::NativeGoQueryHandlerInput = serde_json::from_str(r#"{"id":"18446744073709551615","note":null}"#).unwrap();
 assert_eq!(serde_json::to_value(go).unwrap()["id"], "18446744073709551615");
}
`,
      );
      await writeFile(
        join(rustApp, "examples/tako_schema.rs"),
        'fn main() { tako::schema::Application::new("native-rust").handler::<schema_consumer::Query,schema_consumer::Query>("query").print().unwrap(); }\n',
      );
      await run(rustApp, [binary, "--ci", "gen"]);
      await run(rustApp, [cargo!, "test", "--quiet"]);
      // Import native producer DTOs back into TypeScript without a producer compiler.
      await writeFile(
        join(producer, "tako.toml"),
        'name = "mail"\nruntime = "bun"\napp_root = "."\nschemas = ["../go-app/tako-schema.json", "../rust-app/tako-schema.json"]\n',
      );
      await mkdir(join(producer, "node_modules/.bin"), { recursive: true });
      await symlink(
        join(repo, "sdk/javascript/dist/gen-network-types.mjs"),
        join(producer, "node_modules/.bin/tako-sh-gen-network-types"),
      );
      await run(producer, [binary, "--ci", "gen"]);
      const main = join(producer, "main.ts");
      await writeFile(
        main,
        'import { call } from "tako.sh"; call("native-go.query", { id: "18446744073709551615" }); call("native-rust.query", { id: "u1" });\n// @ts-expect-error uint64 travels as a decimal string\ncall("native-go.query", { id: 42 });\n',
      );
      const program = ts.createProgram([main, join(producer, "tako.d.ts")], {
        strict: true,
        skipLibCheck: true,
        noEmit: true,
        types: [],
        target: ts.ScriptTarget.ESNext,
        module: ts.ModuleKind.ESNext,
        moduleResolution: ts.ModuleResolutionKind.Bundler,
      });
      expect(
        ts
          .getPreEmitDiagnostics(program)
          .map((d) => ts.flattenDiagnosticMessageText(d.messageText, "\n")),
      ).toEqual([]);
      expect(await readFile(join(producer, "tako.imports.ts"), "utf8")).toContain(
        "NativeRustQueryHandler",
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
  180_000,
);
