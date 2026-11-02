import { RedisClient } from "bun";

const redis = new RedisClient(process.env.REDIS_URL ?? "redis://localhost:6379");

Bun.serve({
  hostname: "0.0.0.0",
  port: 3000,
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (request.method !== "GET") {
      return new Response("Method not allowed", { status: 405 });
    }
    if (path !== "/" && path !== "/health") {
      return new Response("Not found", { status: 404 });
    }
    try {
      if (path === "/health") {
        await redis.send("PING", []);
        return new Response("ok");
      }
      const visits = await redis.send("INCR", ["visits"]);
      return Response.json({ message: "Hello from Bun!", visits });
    } catch {
      return new Response("Redis is unavailable", { status: 503 });
    }
  },
});
