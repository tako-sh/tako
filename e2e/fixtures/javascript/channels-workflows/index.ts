import broadcast from "./workflows/broadcast";
import demo from "./channels/demo";

export default async function fetch(request: Request): Promise<Response> {
  const url = new URL(request.url);

  if (url.pathname === "/" && request.method === "GET") {
    return new Response("<!doctype html><html><body><h1>Tako app</h1></body></html>", {
      headers: { "content-type": "text/html; charset=utf-8" },
    });
  }

  if (url.pathname === "/enqueue" && request.method === "POST") {
    const { message } = (await request.json()) as { message: string };
    const runId = await broadcast.enqueue({ message });
    return Response.json({ ok: true, runId });
  }

  if (url.pathname === "/publish" && request.method === "POST") {
    const { message } = (await request.json()) as { message: string };
    const published = await demo.publish({ type: "message", data: { message } });
    return Response.json({ ok: true, id: published.id });
  }

  return new Response("Not Found", { status: 404 });
}
