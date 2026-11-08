import { call, channel, workflow } from "tako.sh";
import relay from "./src/workflows/relay";

export default async function fetch(request: Request): Promise<Response> {
  const { pathname } = new URL(request.url);
  if (request.method !== "POST") return new Response("network caller");
  const { message, id } = (await request.json()) as { message: string; id?: string };
  if (pathname === "/call")
    return Response.json({ ok: true, result: await call("cw-e2e.echo", { message }) });
  if (pathname === "/run")
    return Response.json({ ok: true, result: await relay.run({ message }, { timeout: "20s" }) });
  if (pathname === "/publish") {
    const published = await channel("cw-e2e.demo").publish({ type: "message", data: { message } });
    return Response.json({ ok: true, id: published.id });
  }
  if (pathname === "/enqueue") {
    const runId = await workflow("cw-e2e.broadcast").enqueue(
      { message },
      id === undefined ? {} : { id },
    );
    return Response.json({ ok: true, runId });
  }
  return new Response("Not Found", { status: 404 });
}
