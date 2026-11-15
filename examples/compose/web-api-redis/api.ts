import { RedisClient } from "bun";

const redis = new RedisClient(process.env.REDIS_URL ?? "redis://redis:6379");
const origin = process.env.WEB_URL ?? "http://localhost:8080";

Bun.serve({
  hostname: "0.0.0.0",
  port: 3000,
  async fetch(request) {
    const headers = { "access-control-allow-origin": origin };
    try {
      const path = new URL(request.url).pathname;
      if (path === "/health") {
        await redis.send("PING", []);
        return new Response("ok");
      }
      if (path === "/visits" && request.method === "GET") {
        const visits = Number(await redis.send("INCR", ["visits"]));
        return Response.json({ visits }, { headers });
      }
      return new Response("Not found", { status: 404, headers });
    } catch {
      return new Response("Redis unavailable", { status: 503, headers });
    }
  },
});
