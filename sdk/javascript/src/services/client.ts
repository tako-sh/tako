import type { ServiceDefinition, ServiceInput, ServiceResponse } from "./contract";
import { jsonBody, readJson, readText, ServiceError, validate } from "./http";

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
  [K in keyof S["operations"]]: (
    ...args: {} extends ServiceInput<S["operations"][K]>
      ? [input?: ServiceInput<S["operations"][K]>, options?: ServiceCallOptions]
      : [input: ServiceInput<S["operations"][K]>, options?: ServiceCallOptions]
  ) => Promise<ServiceResponse<S["operations"][K]> & { headers: Headers }>;
};

function assertUrlText(value: string, operation: string, path: string): void {
  if (!value.isWellFormed()) {
    throw new ServiceError(
      "validation",
      operation,
      "request",
      "Invalid Unicode in URL field",
      undefined,
      [{ path, message: "URL fields must contain well-formed Unicode" }],
    );
  }
}

export function createServiceClient<S extends ServiceDefinition>(
  service: S,
  options: ServiceClientOptions,
): ServiceClient<S> {
  const base = new URL(options.baseURL);
  if (
    !/^https?:$/.test(base.protocol) ||
    base.search ||
    base.hash ||
    base.username ||
    base.password
  )
    throw new Error("Service baseURL must be an HTTP URL without query, fragment, or credentials.");
  const fetcher = options.fetch ?? globalThis.fetch;
  const client = Object.create(null) as Record<string, unknown>;
  for (const [name, operation] of Object.entries(service.operations)) {
    client[name] = async (
      input: {
        params?: Record<string, unknown>;
        query?: Record<string, unknown>;
        body?: unknown;
      } = {},
      call: ServiceCallOptions = {},
    ) => {
      const params = validate(operation.params, input.params, name, "request", "params");
      const query = validate(
        operation.query,
        input.query ?? (operation.query ? {} : undefined),
        name,
        "request",
        "query",
      );
      const data = validate(operation.body, input.body, name, "request", "body");
      const url = new URL(base);
      const path = operation.path.replace(/\{([^}]+)\}/g, (_match, key: string) => {
        const value = String(params![key]);
        if (!value || value === "." || value === "..")
          throw new ServiceError(
            "validation",
            name,
            "request",
            "Path params cannot be empty or dot segments",
          );
        assertUrlText(value, name, `/params/${key}`);
        return encodeURIComponent(value);
      });
      url.pathname = base.pathname.replace(/\/$/, "") + path;
      for (const [key, value] of Object.entries(query ?? {})) {
        if (value === undefined) continue;
        if (Array.isArray(value) && !value.length)
          throw new ServiceError(
            "validation",
            name,
            "request",
            `Query array '${key}' must not be empty`,
          );
        const fieldPath = `/query/${key.replaceAll("~", "~0").replaceAll("/", "~1")}`;
        assertUrlText(key, name, fieldPath);
        const values = Array.isArray(value) ? value : [value];
        values.forEach((item, index) => {
          const text = String(item);
          assertUrlText(text, name, Array.isArray(value) ? `${fieldPath}/${index}` : fieldPath);
          url.searchParams.append(key, text);
        });
      }
      const headers = new Headers(options.headers);
      new Headers(call.headers).forEach((value, key) => headers.set(key, value));
      headers.set("accept", "application/json");
      const body = data === undefined ? undefined : jsonBody(data, name, "request");
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
      } catch (cause) {
        throw new ServiceError("transport", name, "request", "HTTP request failed", undefined, [], {
          cause,
        });
      }
      const schema = operation.responses[response.status];
      if (schema === undefined) {
        let cause: unknown;
        try {
          await response.body?.cancel();
        } catch (error) {
          cause = error;
        }
        throw new ServiceError(
          "unexpected-status",
          name,
          "response",
          `Unexpected HTTP status ${response.status}`,
          response.status,
          [],
          { cause },
        );
      }
      let responseData: unknown;
      if (schema === null) {
        if ((await readText(response, name, "response", response.status)).length)
          throw new ServiceError(
            "validation",
            name,
            "response",
            "Expected an empty response",
            response.status,
          );
      } else {
        responseData = validate(
          schema,
          await readJson(response, name, "response", response.status),
          name,
          "response",
          "body",
          response.status,
        );
      }
      return { status: response.status, body: responseData, headers: response.headers };
    };
  }
  return client as ServiceClient<S>;
}
