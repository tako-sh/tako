import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tako } from "tako.sh";
import broadcast from "./src/workflows/broadcast";
import demo from "./src/channels/demo";

const marker = () => join(tako.dataDir, "marker.txt");

export default async function fetch(request: Request): Promise<Response> {
  const url = new URL(request.url);

  if (url.pathname === "/" && request.method === "GET") {
    return Response.json({ app: process.env.TAKO_APP_NAME, build: process.env.TAKO_BUILD });
  }

  if (url.pathname === "/secret" && request.method === "GET") {
    const secret = tako.secrets.SDK_FIXTURE_SECRET;
    return Response.json({ has_secret: typeof secret === "string" && secret.length > 0 });
  }

  if (url.pathname === "/data" && request.method === "POST") {
    const { value } = (await request.json()) as { value: string };
    mkdirSync(tako.dataDir, { recursive: true });
    writeFileSync(marker(), value);
    return Response.json({ ok: true });
  }

  if (url.pathname === "/data" && request.method === "GET") {
    try {
      return Response.json({ value: readFileSync(marker(), "utf8") });
    } catch {
      return Response.json({ value: null });
    }
  }

  if (url.pathname === "/publish" && request.method === "POST") {
    const { message } = (await request.json()) as { message: string };
    const published = await demo.publish({ type: "message", data: { message } });
    return Response.json({ ok: true, id: published.id });
  }

  if (url.pathname === "/enqueue" && request.method === "POST") {
    const { message, id } = (await request.json()) as { message: string; id?: string };
    const runId = await broadcast.enqueue({ message }, id === undefined ? {} : { id });
    return Response.json({ ok: true, runId });
  }

  return new Response("Not Found", { status: 404 });
}
