/**
 * Tako Internal Endpoints
 *
 * These endpoints are handled by the SDK automatically on Host: <app>.tako.
 *
 * - GET  /status — Health/status check
 * - POST /channels/authorize — Channel auth callback
 */

import { ChannelRegistry } from "../channels";
import type { ChannelAuthorizeInput, TakoStatus } from "../types";
import { dispatchWsMessage } from "../channels/handler";
import { getInternalToken, getStorageBindings } from "./secrets";

/** Host suffix used for SDK-internal app requests. */
export const TAKO_INTERNAL_HOST_SUFFIX = ".tako";
/** Built-in health endpoint path on the internal host. */
export const TAKO_INTERNAL_STATUS_PATH = "/status";
/** Channel authorization endpoint path on the internal host. */
export const TAKO_INTERNAL_CHANNELS_AUTHORIZE_PATH = "/channels/authorize";
/** WebSocket channel dispatch endpoint path on the internal host. */
export const TAKO_INTERNAL_CHANNELS_DISPATCH_PATH = "/channels/dispatch";
/** Channel registry endpoint path on the internal host. */
export const TAKO_INTERNAL_CHANNELS_REGISTRY_PATH = "/channels/registry";
/** Header used to authenticate SDK-internal requests. */
export const TAKO_INTERNAL_TOKEN_HEADER = "x-tako-internal-token";
const TAKO_LOCAL_STORAGE_PREFIX = "/_tako/storages/";
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

// CodeQL[js/stack-trace-exposure]: body may carry `err.message` (not stack)
// from user handler exceptions via DispatchResult. This endpoint is gated on
// Host: <app>.tako + internal token — only the tako-server infra reaches
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
  channels: ChannelRegistry = new ChannelRegistry(),
): Promise<Response | null> {
  // Fast path: gate on cheap string checks before constructing a URL, so
  // normal app traffic pays no per-request allocation here.
  const rawUrl = request.url;
  const queryStart = rawUrl.indexOf("?");
  const rawPath = queryStart === -1 ? rawUrl : rawUrl.slice(0, queryStart);
  const hostHeader = normalizeHost(request.headers.get("host"));

  if (rawPath.includes(TAKO_LOCAL_STORAGE_PREFIX)) {
    const url = new URL(rawUrl);
    if (url.pathname.startsWith(TAKO_LOCAL_STORAGE_PREFIX)) {
      return handleLocalStorageRequest(request, url);
    }
  }

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
    case TAKO_INTERNAL_CHANNELS_REGISTRY_PATH:
      return handleChannelRegistry(request, token, channels);

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
      ...(definition.transport !== undefined && { transport: definition.transport }),
    };
    return meta;
  });
  return internalResponse(defs, 200, token);
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
    params?: Record<string, unknown>;
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
    params: body.params ?? {},
    frame: { type: body.frame.type, data: body.frame.data },
    ...(typeof body.subject === "string" && { subject: body.subject }),
  };
  const result = await dispatchWsMessage(channels, input);
  return internalResponse(result, 200, token);
}

/**
 * GET /status on Host: <app>.tako — Full status information
 */
function handleStatus(status: TakoStatus, token: string): Response {
  return internalResponse(status, 200, token);
}

async function handleLocalStorageRequest(request: Request, url: URL): Promise<Response> {
  const route = parseLocalStorageRoute(url.pathname);
  if (route === "invalid") {
    return storageJson({ error: "Invalid key" }, 400);
  }
  if (!route) {
    return storageJson({ error: "Not found" }, 404);
  }
  if (request.method !== "GET" && request.method !== "PUT") {
    return storageJson({ error: "Method not allowed" }, 405);
  }

  const binding = localStorageBinding(route.bindingName);
  if (!binding) {
    return storageJson({ error: "Not found" }, 404);
  }

  const expires = Number(url.searchParams.get("expires"));
  const token = url.searchParams.get("token") ?? "";
  if (!Number.isSafeInteger(expires) || expires < Math.floor(Date.now() / 1000)) {
    return storageJson({ error: "Forbidden" }, 403);
  }

  const payload = `${request.method}\n${route.bindingName}\n${route.encodedKey}\n${expires}`;
  const expectedToken = await hmacHex(utf8(binding.signingKey), payload);
  if (!constantTimeEqual(token, expectedToken)) {
    return storageJson({ error: "Forbidden" }, 403);
  }

  const dataDir = process.env["TAKO_DATA_DIR"];
  if (!dataDir) {
    return storageJson({ error: "Local storage is not available" }, 500);
  }

  const path = await import("node:path");
  const fs = await import("node:fs/promises");
  const root = path.resolve(dataDir, binding.storagePath);
  const target = path.resolve(root, ...route.keySegments);
  if (target !== root && !target.startsWith(root + path.sep)) {
    return storageJson({ error: "Invalid key" }, 400);
  }

  if (request.method === "PUT") {
    await fs.mkdir(path.dirname(target), { recursive: true });
    await fs.writeFile(target, Buffer.from(await request.arrayBuffer()));
    return new Response(null, { status: 204 });
  }

  try {
    const body = request.method === "GET" ? await fs.readFile(target) : null;
    return new Response(body, {
      status: 200,
      headers: { "Content-Type": "application/octet-stream" },
    });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return storageJson({ error: "Not found" }, 404);
    }
    throw error;
  }
}

function parseLocalStorageRoute(pathname: string):
  | {
      bindingName: string;
      encodedKey: string;
      keySegments: string[];
    }
  | "invalid"
  | null {
  const rest = pathname.slice(TAKO_LOCAL_STORAGE_PREFIX.length);
  const [encodedBindingName, ...encodedKeySegments] = rest.split("/");
  if (!encodedBindingName || encodedKeySegments.length === 0) {
    return null;
  }
  const bindingName = safeDecodeURIComponent(encodedBindingName);
  const keySegments = encodedKeySegments.map(safeDecodeURIComponent);
  if (bindingName === null || !keySegments.every(isString)) {
    return "invalid";
  }
  return {
    bindingName,
    encodedKey: encodedKeySegments.join("/"),
    keySegments,
  };
}

function isString(value: string | null): value is string {
  return value !== null;
}

function safeDecodeURIComponent(value: string): string | null {
  try {
    return decodeURIComponent(value);
  } catch {
    return null;
  }
}

function localStorageBinding(
  bindingName: string,
): { storagePath: string; signingKey: string } | null {
  const raw = getStorageBindings()[bindingName];
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return null;
  }
  const binding = raw as { provider?: unknown; path?: unknown; signing_key?: unknown };
  if (
    binding.provider === "local" &&
    typeof binding.path === "string" &&
    binding.path.length > 0 &&
    typeof binding.signing_key === "string" &&
    binding.signing_key.length > 0
  ) {
    return { storagePath: binding.path, signingKey: binding.signing_key };
  }
  return null;
}

function storageJson(body: unknown, status: number): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

async function hmacHex(key: ArrayBuffer, value: string): Promise<string> {
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    key,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const signature = await crypto.subtle.sign("HMAC", cryptoKey, utf8(value));
  return Array.from(new Uint8Array(signature))
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function utf8(value: string): ArrayBuffer {
  return new TextEncoder().encode(value).slice().buffer as ArrayBuffer;
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
