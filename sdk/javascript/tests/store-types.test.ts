import { expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import ts from "typescript";
import { stringifyStoreSnapshot } from "../src/stores/json";
import { defineStore } from "../src/stores/define";
import { setChannelSocketPublisher } from "../src/channels";

test("Store client inference describes serialized JSON rather than backend instances", () => {
  const sdkRoot = resolve(import.meta.dir, "..");
  const directory = mkdtempSync(join(sdkRoot, ".tmp-store-json-"));
  const filename = join(directory, "types.ts");
  writeFileSync(
    filename,
    `
    import type { Jsonify, StoreJsonValue } from "../src/stores/json";
    import { defineStore, type InferStore } from "../src/index";
    type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;
    type Assert<T extends true> = T;
    type DTO = { createdAt: Date; nested: { optional?: Date; missing: undefined; maybe: string | undefined; fn():void; token:symbol }; rows: readonly [Date, undefined, ()=>void, symbol]; };
    export type Cases = [
      Assert<Equal<Jsonify<Date>, string>>,
      Assert<Equal<Jsonify<DTO>, { createdAt:string; nested:{ optional?:string; maybe?:string }; rows:[string,null,null,null] }>>,
      Assert<Equal<Jsonify<{toJSON():{ id:string; at:Date }}>, {id:string;at:string}>>,
      Assert<Equal<Jsonify<{toJSON():{toJSON():string; value:number}}>, {value:number}>>,
      Assert<Equal<Jsonify<Array<string|undefined|(()=>void)>>, Array<string|null>>>,
      Assert<Equal<Jsonify<bigint>, never>>,
      Assert<Equal<Jsonify<()=>void>, never>>,
      Assert<Equal<Jsonify<undefined>, never>>,
      Assert<Equal<Jsonify<unknown>, StoreJsonValue>>,
      Assert<Equal<Jsonify<Map<string,Date>>, Record<string,never>>>,
      Assert<Equal<Jsonify<Set<Date>>, Record<string,never>>>,
      Assert<Equal<Jsonify<Number>, number>>
    ];
    export const messages = defineStore("messages", {
      paramsSchema:t=>t.Object({chatId:t.String()}),
      load:async()=>[{createdAt:new Date(), title:"hello", maybe:undefined as string|undefined}],
    });
    export type Inferred = Assert<Equal<InferStore<typeof messages>, {
      params:{chatId:string};
      data:Array<{createdAt:string;title:string;maybe?:string}>;
    }>>;
    export const untyped = defineStore("untyped", { load:():unknown => null });
    export type Untyped = Assert<Equal<InferStore<typeof untyped>["data"], StoreJsonValue>>;
  `,
  );
  try {
    const configPath = ts.findConfigFile(sdkRoot, ts.sys.fileExists, "tsconfig.json");
    if (!configPath) throw new Error("Missing SDK tsconfig");
    const config = ts.readConfigFile(configPath, ts.sys.readFile);
    if (config.error)
      throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, "\n"));
    const parsed = ts.parseJsonConfigFileContent(
      config.config,
      ts.sys,
      sdkRoot,
      { noEmit: true },
      configPath,
    );
    const program = ts.createProgram([filename], parsed.options);
    const diagnostics = ts.getPreEmitDiagnostics(program);
    expect(
      ts.formatDiagnosticsWithColorAndContext(diagnostics, {
        getCanonicalFileName: (file) => file,
        getCurrentDirectory: () => sdkRoot,
        getNewLine: () => "\n",
      }),
    ).toBe("");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("Store snapshots follow native JSON dates, omission, array nulls, and toJSON semantics", () => {
  let calls = 0;
  const date = new Date("2026-10-25T12:00:00Z");
  const data = {
    createdAt: date,
    missing: undefined,
    fn: () => true,
    symbol: Symbol("hidden"),
    entries: [undefined, () => true, Symbol("hidden"), date],
    map: new Map([["not-enumerable", 1]]),
    set: new Set([1]),
    custom: {
      toJSON() {
        calls++;
        return {
          date,
          toJSON() {
            throw new Error("toJSON must run once per value");
          },
        };
      },
    },
  };
  expect(JSON.parse(stringifyStoreSnapshot({ data }))).toEqual({
    data: {
      createdAt: date.toISOString(),
      entries: [null, null, null, date.toISOString()],
      map: {},
      set: {},
      custom: { date: date.toISOString() },
    },
  });
  expect(calls).toBe(1);
});

test("Store snapshots reject unsupported top-level values, nonfinite numbers, BigInt and cycles", () => {
  for (const data of [undefined, () => true, Symbol("invalid"), { toJSON: () => undefined }]) {
    expect(() => stringifyStoreSnapshot({ data })).toThrow("Store load must return JSON data");
  }
  for (const data of [NaN, Infinity, -Infinity, { value: NaN }, [Infinity], new Number(NaN)]) {
    expect(() => stringifyStoreSnapshot({ data })).toThrow("finite JSON numbers");
  }
  for (const data of [1n, { value: 1n }, [1n]]) {
    expect(() => stringifyStoreSnapshot({ data })).toThrow();
  }
  const cyclic: { self?: unknown } = {};
  cyclic.self = cyclic;
  expect(() => stringifyStoreSnapshot({ data: cyclic })).toThrow();
  expect(JSON.parse(stringifyStoreSnapshot({ data: null }))).toEqual({ data: null });
});

test("Store invalidation rejects nonfinite params before nullable schema conversion or publication", async () => {
  let calls = 0;
  setChannelSocketPublisher(async (channel, payload) => {
    calls++;
    return { id: "1", channel, type: payload.type, data: payload.data };
  });
  try {
    const store = defineStore("numbers", {
      paramsSchema: (t) => t.Object({ n: t.Union([t.Number(), t.Null()]) }),
      load: () => null,
    });
    await expect(store.invalidate({ n: NaN })).rejects.toThrow("finite JSON numbers");
    await expect(store.invalidate({ n: Infinity })).rejects.toThrow("finite JSON numbers");
    expect(calls).toBe(0);
    await store.invalidate({ n: null });
    expect(calls).toBe(1);
  } finally {
    setChannelSocketPublisher(null);
  }
});
