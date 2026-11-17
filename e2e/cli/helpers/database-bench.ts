/**
 * Loopback latency of the hosted database protocol against in-process SQLite.
 *
 *   cargo build --release -p tako-database --example serve
 *   bun e2e/cli/helpers/database-bench.ts target/release/examples/serve
 */
import { Database } from "bun:sqlite";
import { createClient } from "@libsql/client";
import { connect } from "@tursodatabase/serverless";

const serve = process.argv[2];
if (!serve) throw new Error("usage: bun database-bench.ts <path to the serve example>");
const RUNS = 2000;
const WARMUP = 200;

const server = Bun.spawn([serve], { stdin: "pipe", stdout: "pipe" });
const { value } = await server.stdout.getReader().read();
const config = JSON.parse(new TextDecoder().decode(value).split("\n")[0]!) as {
  url: string;
  authToken: string;
  path: string;
};

function report(name: string, samples: number[]): void {
  samples.sort((a, b) => a - b);
  const at = (p: number) => samples[Math.min(samples.length - 1, Math.floor(p * samples.length))]!;
  const us = (ms: number) => `${(ms * 1000).toFixed(0).padStart(6)} µs`;
  console.log(`${name.padEnd(34)} p50 ${us(at(0.5))}   p99 ${us(at(0.99))}`);
}

async function measure(name: string, run: (i: number) => unknown): Promise<void> {
  for (let i = 0; i < WARMUP; i++) await run(i);
  const samples: number[] = [];
  for (let i = 0; i < RUNS; i++) {
    const start = performance.now();
    await run(i);
    samples.push(performance.now() - start);
  }
  report(name, samples);
}

const schema = "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, name TEXT, price REAL)";
const seed = `INSERT INTO items VALUES ${Array.from(
  { length: 1000 },
  (_, i) => `(${i + 1}, 'item ${i + 1}', ${i * 1.5})`,
).join(",")}`;

const libsql = createClient({ url: config.url, authToken: config.authToken });
await libsql.executeMultiple(`${schema}; ${seed};`);
const turso = connect({ url: config.url, authToken: config.authToken });

const local = new Database(`${config.path}.local.sqlite`);
local.run("PRAGMA journal_mode = WAL");
local.run("PRAGMA synchronous = FULL");
local.run(schema);
local.run(seed);
const point = local.prepare("SELECT id, name, price FROM items WHERE id = ?");
const hundred = local.prepare("SELECT id, name, price FROM items WHERE id > ? LIMIT 100");
const insert = local.prepare("INSERT INTO items (name, price) VALUES (?, ?)");

await measure("in process: point select", (i) => point.get((i % 1000) + 1));
await measure("@libsql/client: point select", (i) =>
  libsql.execute({
    sql: "SELECT id, name, price FROM items WHERE id = ?",
    args: [(i % 1000) + 1],
  }),
);
await measure("@tursodatabase/serverless: point", (i) =>
  turso.get("SELECT id, name, price FROM items WHERE id = ?", (i % 1000) + 1),
);
await measure("in process: 100 rows", (i) => hundred.all(i % 900));
await measure("@libsql/client: 100 rows", (i) =>
  libsql.execute({
    sql: "SELECT id, name, price FROM items WHERE id > ? LIMIT 100",
    args: [i % 900],
  }),
);
await measure("in process: insert", (i) => insert.run(`n${i}`, i));
await measure("@libsql/client: insert", (i) =>
  libsql.execute({ sql: "INSERT INTO items (name, price) VALUES (?, ?)", args: [`n${i}`, i] }),
);

libsql.close();
await turso.close();
local.close();
server.stdin.end();
await server.exited;
