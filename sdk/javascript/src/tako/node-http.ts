/**
 * Shared helpers for the Node.js HTTP entrypoints (`node-server` and
 * `node-dev`): bridge `node:http` request/response objects to the Fetch
 * API `Request`/`Response` the Tako runtime expects.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";

export function incomingMessageToRequest(req: IncomingMessage): Request {
  const url = new URL(req.url || "/", `http://${req.headers.host || "localhost"}`);
  const method = req.method || "GET";
  const headers = new Headers();
  for (const [key, value] of Object.entries(req.headers)) {
    if (value === undefined) continue;
    if (Array.isArray(value)) {
      for (const v of value) headers.append(key, v);
    } else {
      headers.set(key, value);
    }
  }

  const hasBody = method !== "GET" && method !== "HEAD";
  const body = hasBody
    ? new ReadableStream({
        start(controller) {
          req.on("data", (chunk: Buffer) => controller.enqueue(chunk));
          req.on("end", () => controller.close());
          req.on("error", (err) => controller.error(err));
        },
      })
    : null;

  return new Request(url.href, { method, headers, body, duplex: "half" } as RequestInit);
}

export async function writeResponse(webResponse: Response, res: ServerResponse): Promise<void> {
  const headers: Record<string, string | string[]> = {};
  webResponse.headers.forEach((value, key) => {
    const existing = headers[key];
    if (existing !== undefined) {
      headers[key] = Array.isArray(existing) ? [...existing, value] : [existing, value];
    } else {
      headers[key] = value;
    }
  });
  res.writeHead(webResponse.status, headers);

  if (!webResponse.body) {
    res.end();
    return;
  }

  const nodeStream = Readable.fromWeb(
    webResponse.body as unknown as import("node:stream/web").ReadableStream,
  );
  // pipeline (unlike pipe) destroys the source when the client disconnects
  // mid-body, cancelling the underlying web stream so user resources backing
  // it are released instead of pending forever.
  try {
    await pipeline(nodeStream, res);
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== "ERR_STREAM_PREMATURE_CLOSE") throw err;
  }
}

/** Start a Node http.Server wired to the given fetch-style handler. */
export function startNodeServer(
  host: string,
  port: number,
  handleRequest: (req: Request) => Promise<Response>,
): Promise<{ actualPort: number; close: () => void }> {
  return new Promise((resolve) => {
    const server = createServer(async (req, res) => {
      try {
        const request = incomingMessageToRequest(req);
        const response = await handleRequest(request);
        await writeResponse(response, res);
      } catch (err) {
        console.error("Error handling request:", err);
        if (!res.headersSent) {
          res.writeHead(500, { "Content-Type": "application/json" });
        }
        res.end(JSON.stringify({ error: "Internal Server Error" }));
      }
    });

    server.listen(port, host, () => {
      const addr = server.address();
      const actualPort = typeof addr === "object" && addr ? addr.port : port;
      resolve({ actualPort, close: () => server.close() });
    });
  });
}
