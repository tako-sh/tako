import { tako } from "tako.sh";

export default async function fetch(request: Request) {
  const url = new URL(request.url);
  if (url.pathname === "/cache" && request.method === "POST") {
    const key = crypto.randomUUID();
    await tako.cache.put(key, { version: 1 }, { ttl: 60_000 });
    await tako.cache.put(key, 2, { ttl: 60_000, shared: true });
    const hit = await tako.cache.get<{ version: number }>(key);
    await tako.cache.delete(key);
    const shared = await tako.cache.get<number>(key, { shared: true });
    await tako.cache.delete(key, { shared: true });
    return Response.json({
      ok:
        hit?.version === 1 &&
        shared === 2 &&
        (await tako.cache.get(key)) === undefined &&
        (await tako.cache.get(key, { shared: true })) === undefined,
    });
  }
  if (url.pathname === "/") {
    return new Response("<!doctype html><html><body><h1>Tako app</h1></body></html>", {
      headers: {
        "content-type": "text/html; charset=utf-8",
      },
    });
  }

  return new Response("Not Found", { status: 404 });
}
