/**
 * Plays every shared workflow scenario in `test-support/workflow-scenarios/`
 * against the real JavaScript worker. The Go SDK plays the same files; the
 * README in that directory defines the format and the matching rules, which
 * this file implements.
 */

import { readdirSync, readFileSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import type { Server, Socket } from "node:net";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { closeInternalSocketPoolsForTests } from "../../src/tako/socket";
import { bootstrapWorker } from "../../src/workflows/bootstrap";
import { workflowsEngine } from "../../src/workflows/engine";
import { WorkflowsClient } from "../../src/workflows/rpc-client";
import { Worker } from "../../src/workflows/worker";
import type { RegisteredWorkflow } from "../../src/workflows/worker";
import { scenarioWorkflows } from "./scenario-workflows";

const SCENARIO_DIR = fileURLToPath(
  new URL("../../../../test-support/workflow-scenarios/", import.meta.url),
);
const APP = "scenario-app";
/** Far longer than any scenario, so a heartbeat is never due. */
const HEARTBEAT_INTERVAL_MS = 3_600_000;

interface Step {
  expect: unknown;
  reply: unknown;
}

interface Scenario {
  description: string;
  drive: "run" | "start";
  workflows: string[];
  steps: Step[];
}

function loadScenario(file: string): Scenario {
  const raw = JSON.parse(readFileSync(join(SCENARIO_DIR, file), "utf8")) as Record<string, unknown>;
  const known = new Set(["description", "drive", "workflows", "steps"]);
  for (const key of Object.keys(raw)) {
    if (!known.has(key)) throw new Error(`${file}: unknown scenario field ${JSON.stringify(key)}`);
  }
  if (raw["drive"] !== "run" && raw["drive"] !== "start") {
    throw new Error(`${file}: unknown drive ${JSON.stringify(raw["drive"])}`);
  }
  const workflows = raw["workflows"];
  if (!Array.isArray(workflows) || workflows.length === 0) {
    throw new Error(`${file}: workflows must be a non-empty list`);
  }
  for (const name of workflows) {
    if (typeof name !== "string" || !(name in scenarioWorkflows)) {
      throw new Error(`${file}: unknown test workflow ${JSON.stringify(name)}`);
    }
  }
  const steps = raw["steps"];
  if (!Array.isArray(steps) || steps.length === 0) {
    throw new Error(`${file}: steps must be a non-empty list`);
  }
  for (const [i, step] of steps.entries()) {
    const keys = Object.keys(step as object).sort();
    if (keys.join(",") !== "expect,reply") {
      throw new Error(`${file}: step ${i} must have exactly "expect" and "reply"`);
    }
  }
  return raw as unknown as Scenario;
}

// --- Matching (rules: test-support/workflow-scenarios/README.md) ---

type Vars = Map<string, unknown>;

class Mismatch extends Error {}

const OPERATORS: Record<string, string[]> = {
  $var: ["$var", "is"],
  $any: ["$any"],
  $type: ["$type"],
  $positive: ["$positive"],
  $contains: ["$contains"],
  $ms_from_now: ["$ms_from_now"],
  $unordered: ["$unordered"],
};

function jsonType(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return jsonType(value) === "object";
}

function operatorOf(expected: Record<string, unknown>, path: string): string | null {
  const dollar = Object.keys(expected).filter((key) => key.startsWith("$"));
  if (dollar.length === 0) return null;
  const op = dollar[0]!;
  const allowed = OPERATORS[op];
  const keys = Object.keys(expected);
  if (
    dollar.length !== 1 ||
    !allowed ||
    !keys.every((key) => allowed.includes(key)) ||
    (op !== "$var" && keys.length !== 1)
  ) {
    throw new Error(`${path}: unknown matcher ${JSON.stringify(expected)}`);
  }
  return op;
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (jsonType(a) !== jsonType(b)) return false;
  if (Array.isArray(a)) {
    const other = b as unknown[];
    return a.length === other.length && a.every((value, i) => deepEqual(value, other[i]));
  }
  if (isPlainObject(a)) {
    const other = b as Record<string, unknown>;
    const keys = Object.keys(a);
    return (
      keys.length === Object.keys(other).length &&
      keys.every((key) => key in other && deepEqual(a[key], other[key]))
    );
  }
  return a === b;
}

function match(expected: unknown, actual: unknown, path: string, vars: Vars, now: number): void {
  const fail = (why: string): never => {
    throw new Mismatch(
      `${path}: ${why}; expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
    );
  };
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual) || actual.length !== expected.length) fail("array differs");
    expected.forEach((item, i) =>
      match(item, (actual as unknown[])[i], `${path}[${i}]`, vars, now),
    );
    return;
  }
  if (!isPlainObject(expected)) {
    if (!deepEqual(expected, actual)) fail("value differs");
    return;
  }
  const op = operatorOf(expected, path);
  switch (op) {
    case null: {
      if (!isPlainObject(actual)) fail("not an object");
      const object = actual as Record<string, unknown>;
      for (const key of Object.keys(expected)) {
        if (!(key in object)) fail(`missing field ${JSON.stringify(key)}`);
        match(expected[key], object[key], `${path}.${key}`, vars, now);
      }
      for (const key of Object.keys(object)) {
        if (!(key in expected)) fail(`unexpected field ${JSON.stringify(key)}`);
      }
      return;
    }
    case "$any":
      return;
    case "$type":
      if (jsonType(actual) !== expected["$type"]) fail("wrong type");
      return;
    case "$positive":
      if (typeof actual !== "number" || !(actual > 0)) fail("not a positive number");
      return;
    case "$contains":
      if (typeof actual !== "string" || !actual.includes(String(expected["$contains"]))) {
        fail("text not contained");
      }
      return;
    case "$ms_from_now": {
      const [min, max] = expected["$ms_from_now"] as [number, number];
      if (typeof actual !== "number") fail("not a number");
      const delta = (actual as number) - now;
      if (delta < min || delta > max) fail(`${delta} ms from now is outside [${min}, ${max}]`);
      return;
    }
    case "$var": {
      const name = String(expected["$var"]);
      if (vars.has(name)) {
        if (!deepEqual(vars.get(name), actual)) {
          fail(`differs from ${name} = ${JSON.stringify(vars.get(name))}`);
        }
        return;
      }
      if ("is" in expected) match(expected["is"], actual, path, vars, now);
      else if (actual === null || actual === undefined) fail("missing value");
      vars.set(name, actual);
      return;
    }
    case "$unordered": {
      const items = expected["$unordered"] as unknown[];
      if (!Array.isArray(actual) || actual.length !== items.length) fail("array differs");
      const assign = (i: number, free: unknown[], current: Vars): Vars | null => {
        if (i === items.length) return current;
        for (const [j, candidate] of free.entries()) {
          const trial = new Map(current);
          try {
            match(items[i], candidate, `${path}{${i}}`, trial, now);
          } catch (err) {
            if (err instanceof Mismatch) continue;
            throw err;
          }
          const done = assign(i + 1, free.toSpliced(j, 1), trial);
          if (done) return done;
        }
        return null;
      };
      const done = assign(0, actual as unknown[], vars);
      if (!done) fail("no ordering matches");
      for (const [key, value] of done!) vars.set(key, value);
      return;
    }
  }
}

// --- Fake internal socket ---

class ScenarioSocket {
  readonly failures: string[] = [];
  private next = 0;
  private readonly vars: Vars = new Map();
  private server!: Server;

  constructor(private readonly steps: Step[]) {}

  get done(): boolean {
    return this.next === this.steps.length;
  }

  async listen(path: string): Promise<void> {
    this.server = createServer((socket: Socket) => {
      let buffer = "";
      socket.on("data", (chunk: Buffer) => {
        buffer += chunk.toString("utf8");
        let newline: number;
        while ((newline = buffer.indexOf("\n")) !== -1) {
          const line = buffer.slice(0, newline);
          buffer = buffer.slice(newline + 1);
          socket.write(`${JSON.stringify(this.answer(JSON.parse(line)))}\n`);
        }
      });
    });
    await new Promise<void>((resolve) => this.server.listen(path, resolve));
  }

  async close(): Promise<void> {
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  private answer(request: unknown): unknown {
    const now = Date.now();
    const index = this.next;
    const step = this.steps[index];
    if (!step) {
      this.failures.push(`unexpected request after the last step: ${JSON.stringify(request)}`);
      return { status: "error", message: "scenario: unexpected request" };
    }
    try {
      match(step.expect, request, `step ${index}`, this.vars, now);
    } catch (err) {
      if (!(err instanceof Mismatch)) throw err;
      this.failures.push(err.message);
      this.next = this.steps.length;
      return { status: "error", message: "scenario: request did not match" };
    }
    this.next++;
    return step.reply;
  }
}

// --- Drives ---

function registryFor(names: string[]): Map<string, RegisteredWorkflow> {
  return new Map(
    names.map((name) => [name, { handler: scenarioWorkflows[name]!.definition.handler }]),
  );
}

async function driveRun(scenario: Scenario, socketPath: string): Promise<void> {
  const worker = new Worker({
    client: new WorkflowsClient(socketPath, APP),
    registry: registryFor(scenario.workflows),
    workerId: "scenario-worker",
    heartbeatIntervalMs: HEARTBEAT_INTERVAL_MS,
  });
  await worker.processOnce();
}

async function driveStart(scenario: Scenario, dir: string): Promise<void> {
  const workflowsDir = join(dir, "app", "workflows");
  await mkdir(workflowsDir, { recursive: true });
  const moduleUrl = new URL("./scenario-workflows.ts", import.meta.url).href;
  for (const name of scenario.workflows) {
    await writeFile(
      join(workflowsDir, `${name}.ts`),
      `import { scenarioWorkflows } from ${JSON.stringify(moduleUrl)};\n` +
        `export default scenarioWorkflows[${JSON.stringify(name)}];\n`,
    );
  }
  // Stop before the claim loop: start scenarios cover what precedes it.
  workflowsEngine.startWorker = (() => {}) as typeof workflowsEngine.startWorker;
  const result = await bootstrapWorker({ appDir: join(dir, "app"), appRoot: "." });
  expect(result).toEqual({ started: true, workflowCount: scenario.workflows.length });
}

const files = readdirSync(SCENARIO_DIR)
  .filter((file) => file.endsWith(".json"))
  .sort();

describe("workflow scenarios", () => {
  let dir: string;
  const savedEnv = { ...process.env };
  const startWorker = workflowsEngine.startWorker.bind(workflowsEngine);

  beforeEach(async () => {
    closeInternalSocketPoolsForTests();
    workflowsEngine._reset();
    dir = await mkdtemp(join("/tmp", "tako-scenario-"));
  });

  afterEach(async () => {
    workflowsEngine.startWorker = startWorker;
    workflowsEngine._reset();
    closeInternalSocketPoolsForTests();
    for (const key of ["TAKO_INTERNAL_SOCKET", "TAKO_APP_NAME"]) {
      if (savedEnv[key] === undefined) delete process.env[key];
      else process.env[key] = savedEnv[key];
    }
    await rm(dir, { recursive: true, force: true });
  });

  test("the scenario directory has scenarios", () => {
    expect(files.length).toBeGreaterThan(0);
  });

  for (const file of files) {
    test(file, async () => {
      const scenario = loadScenario(file);
      const socketPath = join(dir, "internal.sock");
      const socket = new ScenarioSocket(scenario.steps);
      await socket.listen(socketPath);
      try {
        if (scenario.drive === "run") {
          await driveRun(scenario, socketPath);
        } else {
          process.env["TAKO_INTERNAL_SOCKET"] = socketPath;
          process.env["TAKO_APP_NAME"] = APP;
          await driveStart(scenario, dir);
        }
      } finally {
        closeInternalSocketPoolsForTests();
        await socket.close();
      }
      expect(socket.failures).toEqual([]);
      expect(socket.done).toBe(true);
    });
  }
});
