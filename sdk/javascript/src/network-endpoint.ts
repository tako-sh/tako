import { join } from "node:path";
import { Value } from "@sinclair/typebox/value";
import type { TSchema } from "@sinclair/typebox";
import { Channel, definitionLifecycleConfig, type ChannelRegistry } from "./channels";
import { discoverWorkflows } from "./workflows/discovery";
import { WorkflowsClient } from "./workflows/rpc-client";
import { resolveAppRootDir } from "./tako/app-root";
import { discoverNetworkHandlers } from "./network/discovery";
import { TakoError, type TakoErrorReason } from "./tako/error";

/** Dispatch only declared primitives; no handler, transport, or lifecycle supplied by the caller. */
export async function handleNetworkInvocation(
  request: Request,
  channels: ChannelRegistry,
): Promise<Response> {
  const reply = (body: unknown, status = 200) => Response.json(body, { status });
  // The caller's SDK branches on `code`, as on its own internal-socket replies.
  const reject = (status: number, message: string, code?: TakoErrorReason) =>
    reply({ status: "error", message, ...(code !== undefined && { code }) }, status);
  if (request.method !== "POST") return reject(405, "Method not allowed", "invalid_request");
  try {
    const input = (await request.json()) as { name?: unknown; request?: Record<string, unknown> };
    if (
      typeof input.name !== "string" ||
      !/^[A-Za-z0-9_:-]{1,255}$/.test(input.name) ||
      !input.request
    ) {
      return reject(400, "Invalid network invocation", "invalid_request");
    }
    const call = input.request;
    if (call["operation"] === "call") {
      const handlers = await discoverNetworkHandlers(
        join(resolveAppRootDir(process.cwd()), "network-handlers"),
      );
      const handler = handlers.get(input.name);
      if (!handler) return reject(404, "Unknown network handler");
      const data = await handler(call["input"]);
      return reply({ status: "ok", data: data ?? null });
    }
    if (call["operation"] === "result") {
      if (typeof call["id"] !== "string") return reject(400, "Invalid run ID", "invalid_request");
      const client = WorkflowsClient.fromEnv();
      if (!client) return reject(503, "Workflow runtime unavailable", "unavailable");
      return reply({ status: "ok", data: await client.result(input.name, call["id"]) });
    }
    if (call["operation"] === "publish") {
      const entry = channels.all.find(({ name }) => name === input.name);
      if (!entry || input.name.startsWith("tako.")) {
        return reject(404, "Unknown channel", "channel_unknown");
      }
      const params = call["params"] ?? {};
      if (!Value.Check(entry.definition.paramsSchema as TSchema, params)) {
        return reject(400, "Invalid channel params", "channel_params_invalid");
      }
      const message = call["message"] as { type?: unknown; data?: unknown } | undefined;
      if (!message || typeof message.type !== "string" || !("data" in message)) {
        return reject(400, "Invalid channel message", "invalid_request");
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
      if (!definition) return reject(404, "Unknown workflow");
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
        return reject(400, "Invalid enqueue options", "invalid_request");
      }
      const client = WorkflowsClient.fromEnv();
      if (!client) return reject(503, "Workflow runtime unavailable", "unavailable");
      const retries =
        options.max_attempts === undefined ? definition.opts.retries : options.max_attempts - 1;
      const data = await client.enqueue(definition.name, call["payload"], {
        ...(retries !== undefined && { retries }),
        ...(options.run_at_ms !== undefined && { runAt: new Date(options.run_at_ms) }),
        ...(options.id !== undefined && { id: options.id }),
      });
      return reply({ status: "ok", data });
    }
    return reject(400, "Unsupported network operation", "invalid_request");
  } catch (err) {
    // The recipient's own socket rejection (a conflicting run ID, a closed
    // stream) keeps its code; its message stays private to the recipient.
    const reason = err instanceof TakoError ? err.reason : undefined;
    return reject(500, "Network invocation failed", reason);
  }
}
