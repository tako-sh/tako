/**
 * Hosted databases through `tako run`: the CLI serves `.tako/data/databases/`
 * files over the same protocol as tako-server, and both supported JS drivers
 * work against it.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const TAKO_BIN =
  process.env["TAKO_BIN"] ??
  resolve(import.meta.dirname, "..", "..", "..", "target", "debug", "tako");
const HELPERS = resolve(import.meta.dirname, "..", "helpers");
const CLIENTS = join(HELPERS, "database-clients.ts");
const HAS_GO = Bun.which("go") !== null;
// The project stands in for HOME, so keep Go's caches (whose module files
// are read-only) where they already are rather than under the project.
const GO_CACHES: Record<string, string> = HAS_GO
  ? Object.fromEntries(
      ["GOMODCACHE", "GOCACHE", "GOPATH"].map((name) => [
        name,
        Bun.spawnSync(["go", "env", name]).stdout.toString().trim(),
      ]),
    )
  : {};

let project: string;

beforeAll(async () => {
  project = await mkdtemp(join(tmpdir(), "tako-databases-"));
  await writeFile(
    join(project, "tako.toml"),
    [
      'name = "db-app"',
      'runtime = "bun"',
      "",
      "[envs.development]",
      'databases = { main = "app-db", drizzle = "drizzle-db" }',
      "",
      "[envs.production]",
      'route = "db.example.com"',
      'databases = { main = "prod-db" }',
      "",
      "[databases.prod-db]",
      'id = "0123456789abcdef0123456789abcdef"',
      'server = "edge"',
      "",
    ].join("\n"),
  );
});

afterAll(async () => {
  await rm(project, { recursive: true, force: true });
});

async function runClients(command: string[] = ["bun", CLIENTS]): Promise<Record<string, unknown>> {
  const child = Bun.spawn([TAKO_BIN, "run", "--", ...command], {
    cwd: project,
    env: {
      ...process.env,
      ...GO_CACHES,
      HOME: project,
      TAKO_HOME: join(project, ".tako-home"),
      NO_COLOR: "1",
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [code, out, err] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ]);
  if (code !== 0) throw new Error(`tako run exited ${code}\n${out}\n${err}`);
  return JSON.parse(out.trim().split("\n").at(-1)!) as Record<string, unknown>;
}

describe("tako run with a database binding", () => {
  test("serves a local file to @libsql/client and @tursodatabase/serverless", async () => {
    const report = await runClients();

    expect(report["url"]).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);
    expect(existsSync(join(project, ".tako", "data", "databases", "app-db.sqlite"))).toBe(true);
    expect(report["previousRows"]).toBe(0);

    expect(report["insert"]).toEqual({ rowsAffected: 1, hasRowid: true });
    expect(report["blob"]).toEqual([1, 2, 3]);
    expect(report["score"]).toBe(1.5);
    expect(report["kind"]).toBe("blob");
    expect(report["bigint"]).toBe("9007199254740993");

    expect(report["batchRollback"]).toBe("SQLITE_CONSTRAINT");
    expect(report["batchRolledBack"]).toBe(0);
    expect(report["transactions"]).toBe(0);
    expect(report["constraint"]).toBe("SQLITE_CONSTRAINT");
    expect(report["syntax"]).toBe("SQL_PARSE_ERROR");
    expect(report["libsqlErrorClass"]).toBe("LibsqlError");
    expect(report["foreignKeys"]).toBe(1);

    // The database cannot reach other files or load code.
    expect(report["attach"]).not.toBe("ok");
    expect(report["vacuumInto"]).not.toBe("ok");
    expect(report["loadExtension"]).not.toBe("ok");

    expect(report["tursoGet"]).toEqual({ answer: 42 });
    expect(report["tursoAll"]).toBe(2);
    expect(report["tursoRun"]).toBe(1);
    expect(report["tursoIterate"]).toBe(5000);
    expect(report["events"]).toBe(6);
    expect(report["tursoError"]).not.toBe("ok");

    expect(report["badToken"]).not.toBe("ok");
  });

  test("keeps data across runs", async () => {
    const report = await runClients();
    // Each run inserts two users that persist (the insert and the commit).
    expect(report["previousRows"]).toBe(2);
  });

  test("other environments get no databases", async () => {
    const child = Bun.spawn(
      [
        TAKO_BIN,
        "run",
        "--env",
        "production",
        "--",
        "bun",
        "-e",
        "console.log(JSON.stringify(JSON.parse(process.env.TAKO_BOOTSTRAP_DATA).databases ?? {}))",
      ],
      {
        cwd: project,
        env: {
          ...process.env,
          HOME: project,
          TAKO_HOME: join(project, ".tako-home"),
          NO_COLOR: "1",
        },
        stdout: "pipe",
        stderr: "pipe",
      },
    );
    const [code, out, err] = await Promise.all([
      child.exited,
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
    ]);
    expect(code).toBe(0);
    expect(JSON.parse(out.trim())).toEqual({});
    expect(err).toContain("reachable only from deployed apps");
    expect(existsSync(join(project, ".tako", "data", "databases", "prod-db.sqlite"))).toBe(false);
  });

  test("Drizzle migrations and queries run over @libsql/client", async () => {
    const report = await runClients(["bun", join(HELPERS, "drizzle", "run.ts")]);
    expect(report).toEqual({
      published: ["Engines"],
      migrations: 2,
      total: 2,
      afterCascade: 0,
    });
  });

  test.skipIf(!HAS_GO)(
    "the Go SDK and libsql-client-go use the binding",
    async () => {
      const report = await runClients(["go", "-C", join(HELPERS, "go-database"), "run", "."]);
      expect(report).toEqual({
        name: "gopher",
        data: "AQID",
        rolledBack: 0,
        duplicate: true,
        redacted: true,
      });
      // Go's read-only module cache stays outside the removable project.
      expect(existsSync(join(project, "go"))).toBe(false);
    },
    180_000,
  );
});
