import type { ServiceDefinition, ServiceInput, ServiceResponse } from "./contract";
import { jsonBody, readJson, ServiceError, validate } from "./http";

export interface ServiceClientOptions {
  baseURL: string | URL;
  fetch?: (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
  headers?: HeadersInit;
  credentials?: RequestCredentials;
}
export interface ServiceCallOptions {
  headers?: HeadersInit;
  signal?: AbortSignal;
}
export type ServiceClient<S extends ServiceDefinition> = {
  [K in keyof S["operations"]]: (...args: {} extends ServiceInput<S["operations"][K]>
    ? [input?: ServiceInput<S["operations"][K]>, options?: ServiceCallOptions]
    : [input: ServiceInput<S["operations"][K]>, options?: ServiceCallOptions]) => Promise<ServiceResponse<S["operations"][K]> & { headers: Headers }>;
};

export function createServiceClient<S extends ServiceDefinition>(service: S, options: ServiceClientOptions): ServiceClient<S> {
  const base = new URL(options.baseURL);
  if (!/^https?:$/.test(base.protocol) || base.search || base.hash || base.username || base.password) throw new Error("Service baseURL must be an HTTP URL without query, fragment, or credentials.");
  const fetcher = options.fetch ?? globalThis.fetch;
  const client = Object.create(null) as Record<string, unknown>;
  for (const [name, operation] of Object.entries(service.operations)) {
    client[name] = async (input: { params?: Record<string, unknown>; query?: Record<string, unknown>; body?: unknown } = {}, call: ServiceCallOptions = {}) => {
      validate(operation.params, input.params, name, "request", "params");
      validate(operation.query, input.query ?? (operation.query ? {} : undefined), name, "request", "query");
      validate(operation.body, input.body, name, "request", "body");
      const url = new URL(base);
      const path = operation.path.replace(/\{([^}]+)\}/g, (_match, key: string) => {
        const value = String(input.params![key]);
        if (!value || value === "." || value === "..") throw new ServiceError("validation", name, "request", "Path params cannot be empty or dot segments");
        return encodeURIComponent(value);
      });
      url.pathname = base.pathname.replace(/\/$/, "") + path;
      for (const [key, value] of Object.entries(input.query ?? {})) {
        if (value === undefined) continue;
        if (Array.isArray(value) && !value.length) throw new ServiceError("validation", name, "request", `Query array '${key}' must not be empty`);
        for (const item of Array.isArray(value) ? value : [value]) url.searchParams.append(key, String(item));
      }
      const headers = new Headers(options.headers);
      new Headers(call.headers).forEach((value, key) => headers.set(key, value));
      headers.set("accept", "application/json");
      const body = input.body === undefined ? undefined : jsonBody(input.body, name, "request");
      if (body !== undefined) headers.set("content-type", "application/json");
      let response: Response;
      try {
        response = await fetcher(url, {
          method: operation.method,
          headers,
          redirect: "manual",
          ...(body === undefined ? {} : { body }),
          ...(call.signal ? { signal: call.signal } : {}),
          ...(options.credentials ? { credentials: options.credentials } : {}),
        });
      } catch (cause) { throw new ServiceError("transport", name, "request", "HTTP request failed", undefined, [], { cause }); }
      const schema = operation.responses[response.status];
      if (schema === undefined) {
        await response.body?.cancel();
        throw new ServiceError("unexpected-status", name, "response", `Unexpected HTTP status ${response.status}`, response.status);
      }
      let data: unknown;
      if (schema === null) {
        if ((await response.text()).length) throw new ServiceError("validation", name, "response", "Expected an empty response", response.status);
      } else {
        data = await readJson(response, name, "response", response.status);
        validate(schema, data, name, "response", "body", response.status);
      }
      return { status: response.status, body: data, headers: response.headers };
    };
  }
  return client as ServiceClient<S>;
}
