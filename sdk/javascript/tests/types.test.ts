import { describe, expect, test } from "bun:test";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import ts from "typescript";
import type {
  ChannelOperation,
  ChannelSocket,
  ChannelSubscription,
  FetchHandler,
  TakoStatus,
} from "../src/types";
import { defineChannel } from "../src/channels/define";
import type { ChannelDefinition } from "../src/channels/define";
import { defineWorkflow } from "../src/workflows/define";
import type { WorkflowOpts } from "../src/workflows/types";

function expectTypescriptToPass(source: string): void {
  const sdkRoot = resolve(import.meta.dir, "..");
  const filename = resolve(sdkRoot, ".tmp-typecheck", "secret-bag.ts");
  mkdirSync(dirname(filename), { recursive: true });
  writeFileSync(filename, source);

  try {
    const configPath = ts.findConfigFile(sdkRoot, ts.sys.fileExists, "tsconfig.json");
    if (!configPath) throw new Error("Missing sdk/javascript/tsconfig.json");

    const config = ts.readConfigFile(configPath, ts.sys.readFile);
    if (config.error) {
      throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, "\n"));
    }

    const parsed = ts.parseJsonConfigFileContent(
      config.config,
      ts.sys,
      sdkRoot,
      { noEmit: true },
      configPath,
    );
    const program = ts.createProgram([filename], parsed.options);
    const diagnostics = ts.getPreEmitDiagnostics(program);
    const message = ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCanonicalFileName: (file) => file,
      getCurrentDirectory: () => sdkRoot,
      getNewLine: () => "\n",
    });
    expect(message).toBe("");
  } finally {
    rmSync(resolve(sdkRoot, ".tmp-typecheck"), { recursive: true, force: true });
  }
}

describe("Types", () => {
  describe("FetchHandler", () => {
    test("accepts default fetch function handler", () => {
      const handler: FetchHandler = (_request: Request, _env: Record<string, string>) => {
        return new Response("Hello");
      };
      expect(typeof handler).toBe("function");
    });

    test("handler is callable", () => {
      const handler: FetchHandler = (_request: Request, _env: Record<string, string>) =>
        new Response("Hello");
      expect(typeof handler).toBe("function");
    });
  });

  describe("TakoStatus", () => {
    test("accepts healthy status", () => {
      const status: TakoStatus = {
        status: "healthy",
        app: "my-app",
        version: "abc123",
        instance_id: "1",
        pid: 12345,
        uptime_seconds: 100,
      };
      expect(status.status).toBe("healthy");
    });

    test("accepts all status values", () => {
      const statuses: TakoStatus["status"][] = ["healthy", "starting", "draining", "unhealthy"];
      for (const s of statuses) {
        const status: TakoStatus = {
          status: s,
          app: "my-app",
          version: "abc123",
          instance_id: "1",
          pid: 12345,
          uptime_seconds: 100,
        };
        expect(status.status).toBe(s);
      }
    });
  });

  describe("channel types", () => {
    test("accepts channel operations", () => {
      const operations: ChannelOperation[] = ["subscribe", "publish", "connect"];
      expect(operations).toContain("publish");
    });

    test("accepts channel definitions built with defineChannel", () => {
      const exp = defineChannel("chat", {
        paramsSchema: (t) => t.Object({ roomId: t.String() }),
        auth: { verify: async () => true },
        handler: { msg: async (d: { text: string }) => d },
        replayWindowMs: 86_400_000,
        keepaliveIntervalMs: 25_000,
      }).$messageTypes<{ msg: { text: string } }>();
      const definition: ChannelDefinition = exp.definition;

      expect(definition.paramsSchema).toMatchObject({ type: "object" });
      expect(definition.handler).toBeDefined();
      expect(definition.replayWindowMs).toBe(86_400_000);
      expect(definition.keepaliveIntervalMs).toBe(25_000);
    });

    test("distinguishes read-only subscriptions from send-capable sockets", () => {
      const subscription: ChannelSubscription = {
        transport: "sse",
        raw: {},
        close() {},
      };
      const socket: ChannelSocket = {
        transport: "ws",
        raw: {},
        close() {},
        send() {},
      };

      expect(subscription.transport).toBe("sse");
      expect(socket.transport).toBe("ws");
    });
  });

  describe("workflow types", () => {
    test("accepts a worker group in workflow opts", () => {
      const opts = {
        retries: 4,
        worker: "media",
        handler: async (_payload: { imageId: string }, ctx) => {
          ctx.logger.info("processing image");
          await ctx.run("resize", (step) => {
            step.logger.info("resizing", { stepName: step.stepName });
            return step.workflowName;
          });
        },
      } satisfies WorkflowOpts<{ imageId: string }>;
      const workflow = defineWorkflow("process-image", opts);

      expect(workflow.definition.opts.worker).toBe("media");
    });
  });

  describe("secret bag types", () => {
    test("accepts generated secret key maps as the public secret bag shape", () => {
      expectTypescriptToPass(`
        import type { TakoSecretBag } from "../src/index";

        declare module "../src/index" {
          interface TakoSecrets {
            readonly DATABASE_URL: string;
          }
        }

        const secrets: TakoSecretBag = { DATABASE_URL: "postgres://example" };
        const url: string = secrets.DATABASE_URL;
        void url;
      `);
    }, 45_000);
  });
});
