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

  if (url.pathname === "/publish" && request.method === "POST") {
    const { message } = (await request.json()) as { message: string };
    const published = await demo.publish({ type: "message", data: { message } });
    return Response.json({ ok: true, id: published.id });
  }

  if (url.pathname === "/store/messages" && request.method === "POST") {
    if (request.headers.get("authorization") !== "Bearer e2e") {
      return new Response("Forbidden", { status: 403 });
    }
    const { chatId, message } = (await request.json()) as { chatId: string; message: string };
    if (typeof chatId !== "string" || typeof message !== "string") {
      return new Response("Invalid input", { status: 400 });
    }
    appendMessage(chatId, message);
    await messages.invalidate({ chat: { id: chatId } });
    return Response.json({ ok: true });
  }

  return new Response("Not Found", { status: 404 });
}
