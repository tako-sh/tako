import { hostname } from "node:os";
import { signal, tako, WorkflowResultError } from "tako.sh";
import approval from "./src/workflows/approval";
import broadcast from "./src/workflows/broadcast";
import demo from "./src/channels/demo";
import messages, { appendMessage } from "./src/stores/messages";

export default async function fetch(request: Request): Promise<Response> {
  const url = new URL(request.url);

  if (url.pathname === "/" && request.method === "GET") {
    return new Response("<!doctype html><html><body><h1>Tako app</h1></body></html>", {
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  }

  if (url.pathname === "/enqueue" && request.method === "POST") {
    const { message, id } = (await request.json()) as { message: string; id?: string };
    const runId = await broadcast.enqueue({ message }, id === undefined ? {} : { id });
    return Response.json({ ok: true, runId });
  }

  if (url.pathname === "/approval" && request.method === "POST") {
    const { id, settleMs } = (await request.json()) as { id: string; settleMs: number };
    const runId = await approval.enqueue({ settleMs }, { id });
    return Response.json({ ok: true, runId });
  }

  if (url.pathname === "/approval/result" && request.method === "POST") {
    const { id } = (await request.json()) as { id: string };
    try {
      const result = await approval.result(id, { timeout: "1s" });
      return Response.json({ ok: true, status: "succeeded", result });
    } catch (error) {
      if (error instanceof WorkflowResultError) {
        return Response.json({ ok: true, status: error.status });
      }
      throw error;
    }
  }

  if (url.pathname === "/signal" && request.method === "POST") {
    const { runId, event, payload } = (await request.json()) as {
      runId: string;
      event: string;
      payload: unknown;
    };
    const status = await signal(event, payload, { runId });
    return Response.json({ ok: true, status });
  }

  if (url.pathname === "/publish" && request.method === "POST") {
    const { message } = (await request.json()) as { message: string };
    const published = await demo.publish({ type: "message", data: { message } });
    return Response.json({
      ok: true,
      id: published.id,
      publisher: hostname(),
      databaseBindings: Object.keys(tako.databases),
      hasReplayBinding: tako.databases["@channels"] !== undefined,
    });
  }

  if (url.pathname === "/store/messages" && request.method === "POST") {
    if (request.headers.get("authorization") !== "Bearer e2e") {
      return new Response("Forbidden", { status: 403 });
    }
    const { chatId, message } = (await request.json()) as { chatId: string; message: string };
    if (typeof chatId !== "string" || typeof message !== "string") {
      return new Response("Invalid input", { status: 400 });
    }
    await appendMessage(chatId, message);
    await messages.invalidate({ chat: { id: chatId } });
    return Response.json({ ok: true });
  }

  return new Response("Not Found", { status: 404 });
}
