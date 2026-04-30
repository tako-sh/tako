/**
 * Tako Internal Endpoints
 *
 * These endpoints are handled by the SDK automatically on Host: tako.internal.
 *
 * - GET  /status — Health/status check
 * - POST /channels/authorize — Channel auth callback
 */

import type { ChannelRegistry } from "../channels";
import type { ChannelAuthorizeInput, TakoStatus } from "../types";
import { dispatchWsMessage } from "../channels/handler";
import { getInternalToken } from "./secrets";

export const TAKO_INTERNAL_HOST = "tako.internal";
export const TAKO_INTERNAL_STATUS_PATH = "/status";
export const TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH = "/channels/authorize";
export const TAKO_INTERNAL_CHANNELS_DISPATCH_PATH = "/channels/dispatch";
export const TAKO_INTERNAL_TOKEN_HEADER = "x-tako-internal-token";
const LOOPBACK_INTERNAL_HOSTS = new Set(["127.0.0.1", "localhost", "0.0.0.0"]);

function normalizeHost(value: string | null): string | null {
  if (!value) {
    return null;
  }
  const normalized = value.trim().toLowerCase();
  if (normalized.length === 0) {
    return null;
  }
  const [host = ""] = normalized.split(":");
  return host;
}

function isInternalHost(host: string | null): boolean {
  if (!host) {
    return false;
  }
  return host === TAKO_INTERNAL_HOST || LOOPBACK_INTERNAL_HOSTS.has(host);
}

function internalToken(): string | null {
  return getInternalToken();
}

// CodeQL[js/stack-trace-exposure]: body may carry `err.message` (not stack)
// from user handler exceptions via DispatchResult. This endpoint is gated on
// Host: tako.internal + internal token — only the tako-server infra reaches
// it, and the server uses the error string for operator logging, never
// forwarding it to external WS clients.
function internalResponse(
  body: unknown,
  status: number,
  token: string,
  extraHeaders?: Record<string, string>,
): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "Content-Type": "application/json",
      [TAKO_INTERNAL_TOKEN_HEADER]: token,
      ...extraHeaders,
    },
  });
}

/**
 * Handle Tako internal endpoints (internal host only).
 *
 * Returns a Response for internal requests, or null for non-internal requests.
 */
export async function handleTakoEndpoint(
  request: Request,
  status: TakoStatus,
  channels: ChannelRegistry,
): Promise<Response | null> {
  // Fast path: check Host header before parsing the URL (avoids allocation for normal traffic)
  const hostHeader = normalizeHost(request.headers.get("host"));
  if (hostHeader && !isInternalHost(hostHeader)) {
    return null;
  }

  const url = new URL(request.url);
  const host = hostHeader || normalizeHost(url.host);
  if (!isInternalHost(host)) {
    return null;
  }

  const token = internalToken();
  const path = url.pathname;
  if (!token || request.headers.get(TAKO_INTERNAL_TOKEN_HEADER) !== token) {
    return new Response(JSON.stringify({ error: "Forbidden" }), {
      status: 403,
      headers: { "Content-Type": "application/json" },
    });
  }

  switch (path) {
    case TAKO_INTERNAL_STATUS_PATH:
      return handleStatus(status, token);
    case TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH:
      return await handleChannelAuthorize(request, token, channels);
    case TAKO_INTERNAL_CHANNELS_DISPATCH_PATH:
      return await handleChannelDispatch(request, token, channels);

    default:
      return internalResponse({ error: "Not found" }, 404, token);
  }
}

async function handleChannelDispatch(
  request: Request,
  token: string,
  channels: ChannelRegistry,
): Promise<Response> {
  if (request.method !== "POST") {
    return internalResponse({ error: "Method not allowed" }, 405, token);
  }

  type DispatchBody = {
    channel?: string;
    frame?: { type?: string; data?: unknown };
    subject?: string;
  };

  let body: DispatchBody;
  try {
    body = (await request.json()) as DispatchBody;
  } catch {
    return internalResponse({ error: "Invalid JSON" }, 400, token);
  }

  if (
    typeof body.channel !== "string" ||
    !body.frame ||
    typeof body.frame.type !== "string" ||
    !("data" in body.frame)
  ) {
    return internalResponse({ error: "Invalid request" }, 400, token);
  }

  const input = {
    channel: body.channel,
    frame: { type: body.frame.type, data: body.frame.data },
    ...(typeof body.subject === "string" && { subject: body.subject }),
  };
  const result = await dispatchWsMessage(channels, input);
  return internalResponse(result, 200, token);
}

/**
 * GET /status on Host: tako.internal — Full status information
 */
function handleStatus(status: TakoStatus, token: string): Response {
  return internalResponse(status, 200, token);
}

async function handleChannelAuthorize(
  request: Request,
  token: string,
  channels: ChannelRegistry,
): Promise<Response> {
  if (request.method !== "POST") {
    return internalResponse({ error: "Method not allowed" }, 405, token);
  }

  let input: ChannelAuthorizeInput;
  try {
    input = (await request.json()) as ChannelAuthorizeInput;
  } catch {
    return internalResponse({ error: "Invalid JSON", ok: false }, 400, token);
  }

  if (!input.channel || !input.operation || input.params === undefined) {
    return internalResponse({ error: "Invalid request", ok: false }, 400, token);
  }

  const result = await channels.authorize(input);
  if (!result.ok) {
    const hasDefinition = channels.resolve(input.channel) !== null;
    if (!hasDefinition) {
      return internalResponse({ error: "Channel not defined", ok: false }, 404, token);
    }
    if (result.reason === "sse_publish_not_allowed") {
      return internalResponse(
        { error: "Method not allowed", ok: false, reason: result.reason },
        405,
        token,
      );
    }
    return internalResponse({ error: "Forbidden", ok: false }, 403, token);
  }

  return internalResponse(result, 200, token);
}
