/**
 * Tako Internal Endpoints
 *
 * These endpoints are handled by the SDK automatically on Host: <app>.tako.
 *
 * - GET  /status — Health/status check
 * - POST /channels/authorize — Channel auth callback
 */

import { ChannelRegistry, definitionLifecycleConfig } from "../channels";
import type { ChannelAuthorizeInput, TakoStatus } from "../types";
import { getInternalToken } from "./secrets";
import { StoreRegistry } from "../stores/registry";
import { handleStoreLoad } from "../stores/endpoint";
import { handleNetworkInvocation } from "../network-endpoint";

/** Host suffix used for SDK-internal app requests. */
export const TAKO_INTERNAL_HOST_SUFFIX = ".tako";
/** Built-in health endpoint path on the internal host. */
export const TAKO_INTERNAL_STATUS_PATH = "/status";
/** Channel authorization endpoint path on the internal host. */
export const TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH = "/channels/authorize";
/** Channel registry endpoint path on the internal host. */
export const TAKO_INTERNAL_CHANNELS_REGISTRY_PATH = "/channels/registry";
/** Header used to authenticate SDK-internal requests. */
export const TAKO_INTERNAL_TOKEN_HEADER = "x-tako-internal-token";
const LOOPBACK_INTERNAL_HOSTS = new Set(["127.0.0.1", "localhost", "0.0.0.0"]);

function baseAppName(value: string): string {
  const [appName = ""] = value.trim().toLowerCase().split("/");
  return appName.length > 0 ? appName : "app";
}

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

/** Return the SDK-internal host for an app name. */
export function internalAppHost(appName: string): string {
  return `${baseAppName(appName)}${TAKO_INTERNAL_HOST_SUFFIX}`;
}

function runtimeAppName(): string | null {
  const processLike = globalThis as typeof globalThis & {
    process?: { env?: Record<string, string | undefined> };
  };
  const appName = processLike.process?.env?.["TAKO_APP_NAME"]?.trim();
  return appName && appName.length > 0 ? appName : null;
}

function isInternalHost(host: string | null): boolean {
  if (!host) {
    return false;
  }
  if (LOOPBACK_INTERNAL_HOSTS.has(host)) {
    return true;
  }
  const appName = runtimeAppName();
  if (appName) {
    return host === internalAppHost(appName);
  }
  return host.endsWith(TAKO_INTERNAL_HOST_SUFFIX) && host.length > TAKO_INTERNAL_HOST_SUFFIX.length;
}

function internalToken(): string | null {
  return getInternalToken();
}

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
  channels: ChannelRegistry = new ChannelRegistry(),
  stores: StoreRegistry = new StoreRegistry(),
): Promise<Response | null> {
  // Fast path: gate on cheap string checks before constructing a URL, so
  // normal app traffic pays no per-request allocation here.
  const rawUrl = request.url;
  const hostHeader = normalizeHost(request.headers.get("host"));

  if (hostHeader && !isInternalHost(hostHeader)) {
    return null;
  }

  const url = new URL(rawUrl);
  const host = hostHeader || normalizeHost(url.host);
  if (!isInternalHost(host)) {
    return null;
  }

  const token = internalToken();
  const path = url.pathname;
  const provided = request.headers.get(TAKO_INTERNAL_TOKEN_HEADER) ?? "";
  if (!token || !constantTimeEqual(provided, token)) {
    return new Response(JSON.stringify({ error: "Forbidden" }), {
      status: 403,
      headers: { "Content-Type": "application/json" },
    });
  }

  switch (path) {
    case "/network": {
      const response = await handleNetworkInvocation(request, channels);
      response.headers.set(TAKO_INTERNAL_TOKEN_HEADER, token);
      return response;
    }
    case TAKO_INTERNAL_STATUS_PATH:
      return handleStatus(status, token);
    case TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH:
      return await handleChannelAuthorize(request, token, channels);
    case TAKO_INTERNAL_CHANNELS_REGISTRY_PATH:
      return handleChannelRegistry(request, token, channels);
    case "/stores/load": {
      const response = await handleStoreLoad(request, stores);
      response.headers.set(TAKO_INTERNAL_TOKEN_HEADER, token);
      return response;
    }

    default:
      return internalResponse({ error: "Not found" }, 404, token);
  }
}

function handleChannelRegistry(
  request: Request,
  token: string,
  channels: ChannelRegistry,
): Response {
  if (request.method !== "GET") {
    return internalResponse({ error: "Method not allowed" }, 405, token);
  }

  const defs = channels.all.map(({ name, definition }) => {
    const meta: {
      channel: string;
      paramsSchema: object;
      auth: false | { headerName?: string | false; cookieName?: string };
      transport?: "ws";
    } = {
      ...definitionLifecycleConfig(definition),
      channel: name,
      paramsSchema: definition.paramsSchema,
      auth:
        definition.auth === false
          ? false
          : {
              ...(definition.auth.headerName !== undefined && {
                headerName: definition.auth.headerName,
              }),
              ...(definition.auth.cookieName !== undefined && {
                cookieName: definition.auth.cookieName,
              }),
            },
      ...(definition.transport !== undefined && {
        transport: definition.transport,
      }),
    };
    return meta;
  });
  return internalResponse(defs, 200, token);
}

/**
 * GET /status on Host: <app>.tako — Full status information
 */
function handleStatus(status: TakoStatus, token: string): Response {
  return internalResponse(status, 200, token);
}

function constantTimeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
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
