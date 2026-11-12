import { join } from "node:path";
import { Value } from "@sinclair/typebox/value";
import type { TSchema } from "@sinclair/typebox";
import { Channel, definitionLifecycleConfig, type ChannelRegistry } from "./channels";
import { discoverWorkflows } from "./workflows/discovery";
import { WorkflowsClient } from "./workflows/rpc-client";
import { resolveAppRootDir } from "./tako/app-root";
import { discoverNetworkHandlers } from "./network/discovery";

/** Dispatch only declared primitives; no handler, transport, or lifecycle supplied by the caller. */
export async function handleNetworkInvocation(
  request: Request,
  channels: ChannelRegistry,
): Promise<Response> {
  const reply = (body: unknown, status = 200) => Response.json(body, { status });
  if (request.method !== "POST")
    return reply({ status: "error", message: "Method not allowed" }, 405);
  try {
    const input = (await request.json()) as { name?: unknown; request?: Record<string, unknown> };
    if (
      typeof input.name !== "string" ||
      !/^[A-Za-z0-9_:-]{1,255}$/.test(input.name) ||
      !input.request
    ) {
      return reply({ status: "error", message: "Invalid network invocation" }, 400);
    }
    const call = input.request;
    if (call["operation"] === "call") {
      const handlers = await discoverNetworkHandlers(
        join(resolveAppRootDir(process.cwd()), "network-handlers"),
      );
      const handler = handlers.get(input.name);
      if (!handler) return reply({ status: "error", message: "Unknown network handler" }, 404);
      const data = await handler(call["input"]);
      return reply({ status: "ok", data: data ?? null });
    }
    if (call["operation"] === "result") {
      if (typeof call["id"] !== "string")
        return reply({ status: "error", message: "Invalid run ID" }, 400);
      const client = WorkflowsClient.fromEnv();
      if (!client) return reply({ status: "error", message: "Workflow runtime unavailable" }, 503);
      return reply({ status: "ok", data: await client.result(input.name, call["id"]) });
    }
    if (call["operation"] === "publish") {
      const entry = channels.all.find(({ name }) => name === input.name);
      if (!entry || input.name.startsWith("tako.")) {
        return reply({ status: "error", message: "Unknown channel" }, 404);
      }
      const params = call["params"] ?? {};
      if (!Value.Check(entry.definition.paramsSchema as TSchema, params)) {
        return reply({ status: "error", message: "Invalid channel params" }, 400);
      }
      const message = call["message"] as { type?: unknown; data?: unknown } | undefined;
      if (!message || typeof message.type !== "string" || !("data" in message)) {
        return reply({ status: "error", message: "Invalid channel message" }, 400);
      }
      const channel = new Channel(
        entry.name,
        entry.definition.transport,
        params as Record<string, unknown>,
        definitionLifecycleConfig(entry.definition),
        entry.definition.paramsSchema,
      );
      // Params select the recipient stream, as `channel(params).publish` does.
      const data = await channel.publish({ type: message.type, data: message.data });
      return reply({ status: "ok", data });
    }
    if (call["operation"] === "enqueue") {
      const workflows = await discoverWorkflows(
        join(resolveAppRootDir(process.cwd()), "workflows"),
      );
      const definition = workflows.find(({ name }) => name === input.name);
      if (!definition) return reply({ status: "error", message: "Unknown workflow" }, 404);
      const options = (call["opts"] ?? {}) as {
        run_at_ms?: number;
        max_attempts?: number;
        id?: string;
      };
      if (
        (options.max_attempts !== undefined &&
          (!Number.isInteger(options.max_attempts) ||
            options.max_attempts < 1 ||
            options.max_attempts > 0xffffffff)) ||
        (options.run_at_ms !== undefined && !Number.isSafeInteger(options.run_at_ms)) ||
        (options.id !== undefined && typeof options.id !== "string")
      ) {
        return reply({ status: "error", message: "Invalid enqueue options" }, 400);
      }
      const client = WorkflowsClient.fromEnv();
      if (!client) return reply({ status: "error", message: "Workflow runtime unavailable" }, 503);
      const retries =
        options.max_attempts === undefined ? definition.opts.retries : options.max_attempts - 1;
      const data = await client.enqueue(definition.name, call["payload"], {
        ...(retries !== undefined && { retries }),
        ...(options.run_at_ms !== undefined && { runAt: new Date(options.run_at_ms) }),
        ...(options.id !== undefined && { id: options.id }),
      });
      return reply({ status: "ok", data });
    }
    return reply({ status: "error", message: "Unsupported network operation" }, 400);
  } catch {
    return reply({ status: "error", message: "Network invocation failed" }, 500);
  }
}
