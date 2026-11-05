import { OptionalKind, type TObject } from "@sinclair/typebox";
import type { ServiceDefinition } from "./contract";

export type ServiceJson =
  | null
  | boolean
  | number
  | string
  | ServiceJson[]
  | { [key: string]: ServiceJson };

export function serializeServiceDocument(value: unknown): string {
  return (
    JSON.stringify(
      value,
      (_key, child: unknown) => {
        if (child !== null && typeof child === "object" && !Array.isArray(child)) {
          return Object.fromEntries(
            Object.entries(child).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
          );
        }
        return child;
      },
      2,
    ) + "\n"
  );
}

export function serviceManifest(service: ServiceDefinition): {
  name: string;
  operations: ServiceJson;
} {
  const operations = Object.fromEntries(
    Object.entries(service.operations).map(([name, operation]) => [
      name,
      { ...operation, ...(operation.body ? { bodyRequired: !operation.body[OptionalKind] } : {}) },
    ]),
  );
  return JSON.parse(serializeServiceDocument({ name: service.name, operations })) as {
    name: string;
    operations: ServiceJson;
  };
}

export function serviceOpenApi(services: readonly ServiceDefinition[]) {
  const paths: Record<string, Record<string, unknown>> = Object.create(null) as Record<
    string,
    Record<string, unknown>
  >;
  const ids = new Set<string>();
  const routes = new Set<string>();
  for (const service of services) {
    for (const [name, operation] of Object.entries(service.operations)) {
      const operationId = `${service.name}.${name}`;
      if (ids.has(operationId)) throw new Error(`Duplicate operation ID '${operationId}'.`);
      ids.add(operationId);
      const route = `${operation.method} ${operation.path.replace(/\{[^}]+\}/g, "{}")}`;
      if (routes.has(route)) throw new Error(`Duplicate HTTP route '${route}'.`);
      routes.add(route);
      const parameters = (
        [
          ["path", operation.params],
          ["query", operation.query],
        ] as const
      ).flatMap(([location, schema]) =>
        Object.entries((schema as TObject | undefined)?.properties ?? {}).map(([key, field]) => ({
          name: key,
          in: location,
          required: location === "path" || !!schema?.required?.includes(key),
          schema: field,
          ...(location === "query" ? { style: "form", explode: true } : { style: "simple" }),
        })),
      );
      const responses = Object.fromEntries(
        Object.entries(operation.responses).map(([status, schema]) => [
          status,
          {
            description: `HTTP ${status}`,
            ...(schema === null ? {} : { content: { "application/json": { schema } } }),
          },
        ]),
      );
      const path = (paths[operation.path] ??= {});
      path[operation.method.toLowerCase()] = {
        operationId,
        parameters,
        ...(operation.body
          ? {
              requestBody: {
                required: !operation.body[OptionalKind],
                content: { "application/json": { schema: operation.body } },
              },
            }
          : {}),
        responses,
      };
    }
  }
  return JSON.parse(
    serializeServiceDocument({
      openapi: "3.1.0",
      info: {
        title:
          services
            .map((service) => service.name)
            .sort()
            .join(", ") || "Services",
        version: "1.0.0",
      },
      paths,
    }),
  ) as ServiceJson;
}
