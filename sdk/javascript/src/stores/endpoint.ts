import { createLogger } from "../logger";
import type { StoreRegistry } from "./registry";
import { stringifyStoreSnapshot } from "./json";

const MAX_BYTES = 1024 * 1024;
const logger = createLogger("stores");

/** Only called after internal host and bootstrap-token authentication. */
export async function handleStoreLoad(request: Request, stores: StoreRegistry): Promise<Response> {
  const json = (body: unknown, status = 200) =>
    Response.json(body, {
      status,
      headers: { "Cache-Control": "no-store" },
    });
  if (request.method !== "POST") return json({ error: "Method not allowed" }, 405);
  let input: { store: string; params: unknown; subject?: string };
  try {
    // Read with a hard cap instead of buffering an arbitrarily large request.
    const reader = request.body?.getReader();
    if (!reader) return json({ error: "Invalid request" }, 400);
    const chunks: Uint8Array[] = [];
    let size = 0;
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > MAX_BYTES) {
          await reader.cancel();
          return json({ error: "Invalid request" }, 400);
        }
        chunks.push(value);
      }
    } finally {
      reader.releaseLock();
    }
    const body = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      body.set(chunk, offset);
      offset += chunk.byteLength;
    }
    input = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body)) as typeof input;
    if (
      !input ||
      typeof input !== "object" ||
      typeof input.store !== "string" ||
      !input.store ||
      !input.params ||
      typeof input.params !== "object" ||
      Array.isArray(input.params) ||
      (input.subject !== undefined && typeof input.subject !== "string")
    ) {
      return json({ error: "Invalid request" }, 400);
    }
  } catch {
    return json({ error: "Invalid request" }, 400);
  }
  const store = stores.find(input.store);
  if (!store) return json({ error: "Store not defined" }, 404);
  try {
    const data = await store.load({
      params: input.params,
      ...(input.subject !== undefined && { subject: input.subject }),
      signal: request.signal,
    });
    if (data === undefined)
      throw new Error("Store load must return JSON data; use null for an empty result.");
    const encoded = stringifyStoreSnapshot({
      data,
      ...(store.offline && { offline: store.offline }),
    });
    if (new TextEncoder().encode(encoded).length > MAX_BYTES)
      throw new Error("Store response exceeds 1 MiB.");
    return new Response(encoded, {
      headers: {
        "Content-Type": "application/json",
        "Cache-Control": "no-store",
      },
    });
  } catch (error) {
    logger.error("Store load failed", { store: input.store, error });
    return json({ error: "Internal Server Error" }, 500);
  }
}
