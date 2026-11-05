import { OptionalKind } from "@sinclair/typebox";
import type { ServiceDefinition, ServiceInput, ServiceResponse } from "./contract";
import { decodeQuery, jsonBody, readJson, ServiceError, validate } from "./http";

export type ServiceHandlers<S extends ServiceDefinition> = {
  [K in keyof S["operations"]]: (
    input: ServiceInput<S["operations"][K]>,
    request: Request,
  ) => ServiceResponse<S["operations"][K]> | Promise<ServiceResponse<S["operations"][K]>>;
};

export function createServiceHandler<
  S extends ServiceDefinition,
  const H extends ServiceHandlers<S>,
>(
  service: S,
  handlers: H,
  options: { basePath?: string } = {},
): (request: Request) => Promise<Response> {
  const basePath = options.basePath?.replace(/\/$/, "") ?? "";
  if (basePath && (!basePath.startsWith("/") || /[?#{}]/.test(basePath)))
    throw new Error("basePath must be an absolute URL path.");
  const routes = Object.entries(service.operations)
    .map(([name, operation]) => ({ name, operation, segments: operation.path.split("/") }))
    .sort(
      (a, b) =>
        a.segments.filter((part) => part.startsWith("{")).length -
        b.segments.filter((part) => part.startsWith("{")).length,
    );
  for (const { name } of routes)
    if (typeof handlers[name] !== "function") throw new Error(`Missing handler '${name}'.`);
  return async (request) => {
    const url = new URL(request.url);
    if (basePath && !url.pathname.startsWith(`${basePath}/`))
      return new Response(null, { status: 404 });
    const segments = url.pathname.slice(basePath.length).split("/");
    const allowed: string[] = [];
    for (const { name, operation, segments: route } of routes) {
      if (
        segments.length !== route.length ||
        route.some((part, index) => !part.startsWith("{") && part !== segments[index])
      )
        continue;
      allowed.push(operation.method);
      if (operation.method !== request.method) continue;
      let input: {
        params?: Record<string, string>;
        query?: Record<string, unknown>;
        body?: unknown;
      };
      try {
        const params: Record<string, string> = Object.create(null) as Record<string, string>;
        route.forEach((part, index) => {
          if (part.startsWith("{")) {
            let value: string;
            try {
              value = decodeURIComponent(segments[index]!);
            } catch {
              throw new ServiceError("validation", name, "request", "Malformed path encoding");
            }
            if (!value || value === "." || value === "..")
              throw new ServiceError("validation", name, "request", "Invalid path param");
            params[part.slice(1, -1)] = value;
          }
        });
        const query = decodeQuery(operation.query, url.searchParams);
        const body = await readJson(request, name, "request");
        validate(
          operation.params,
          operation.params ? params : undefined,
          name,
          "request",
          "params",
        );
        validate(operation.query, query, name, "request", "query");
        if (!(body === undefined && operation.body?.[OptionalKind]))
          validate(operation.body, body, name, "request", "body");
        input = {
          ...(operation.params ? { params } : {}),
          ...(query ? { query } : {}),
          ...(body === undefined ? {} : { body }),
        };
      } catch (error) {
        if (!(error instanceof ServiceError)) throw error;
        return Response.json(
          { error: error.code, message: error.message, issues: error.issues },
          { status: 400 },
        );
      }
      const result = await handlers[name]!(input as ServiceInput<S["operations"][string]>, request);
      const schema = operation.responses[result.status];
      if (schema === undefined)
        throw new ServiceError(
          "unexpected-status",
          name,
          "response",
          `Handler returned undeclared status ${result.status}`,
          result.status,
        );
      validate(schema ?? undefined, result.body, name, "response", "body", result.status);
      const headers = new Headers(result.headers);
      if (schema === null) return new Response(null, { status: result.status, headers });
      headers.set("content-type", "application/json");
      return new Response(jsonBody(result.body, name, "response", result.status), {
        status: result.status,
        headers,
      });
    }
    return new Response(null, {
      status: allowed.length ? 405 : 404,
      ...(allowed.length ? { headers: { allow: [...new Set(allowed)].join(", ") } } : {}),
    });
  };
}
