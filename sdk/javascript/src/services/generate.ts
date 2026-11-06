import { Kind, OptionalKind, ReadonlyKind, type TSchema } from "@sinclair/typebox";
import type { ServiceDefinition } from "./contract";
import { serializeServiceDocument, serviceManifest, serviceOpenApi } from "./manifest";

function schemaSource(schema: TSchema): string {
  const kind = schema[Kind];
  const structural = new Set([
    "type",
    "properties",
    "required",
    "items",
    "anyOf",
    "allOf",
    "const",
    "additionalProperties",
  ]);
  const options = Object.fromEntries(
    Object.entries(schema).filter(([key]) => !structural.has(key)),
  );
  if (schema["additionalProperties"] !== undefined) {
    const additional = schema["additionalProperties"] as boolean | TSchema;
    options["additionalProperties"] = additional;
  }
  const optionEntries = Object.entries(options)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(
      ([key, value]) =>
        `[${JSON.stringify(key)}]: ${key === "additionalProperties" && typeof value === "object" ? schemaSource(value as TSchema) : serializeServiceDocument(value).trim()}`,
    );
  const optionSource = `{ ${optionEntries.join(", ")} }`;
  let source: string;
  if (kind === "Object") {
    const properties = Object.entries(schema["properties"] as Record<string, TSchema>)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([name, field]) => `[${JSON.stringify(name)}]: ${schemaSource(field)}`);
    source = `t.Object({ ${properties.join(", ")} }, ${optionSource})`;
  } else if (kind === "Array")
    source = `t.Array(${schemaSource(schema["items"] as TSchema)}, ${optionSource})`;
  else if (kind === "Union" || kind === "Intersect")
    source = `t.${kind}([${(schema[kind === "Union" ? "anyOf" : "allOf"] as TSchema[]).map(schemaSource).join(", ")}], ${optionSource})`;
  else if (kind === "Literal")
    source = `t.Literal(${JSON.stringify(schema["const"])}, ${optionSource})`;
  else source = `t.${kind}(${optionSource})`;
  if (schema[OptionalKind]) source = `t.Optional(${source})`;
  if (schema[ReadonlyKind]) source = `t.Readonly(${source})`;
  return source;
}

export type ServiceOutputFormat = "typescript" | "manifest" | "openapi";

export function generateServiceDocument(
  services: readonly ServiceDefinition[],
  format: ServiceOutputFormat = "typescript",
): string {
  const sorted = [...services].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  if (new Set(sorted.map((service) => service.name)).size !== sorted.length)
    throw new Error("Duplicate service name.");
  if (format === "manifest")
    return serializeServiceDocument({ services: sorted.map(serviceManifest) });
  if (format === "openapi") return serializeServiceDocument(serviceOpenApi(sorted));
  const declarations = sorted.map((service) => {
    const operations = Object.entries(service.operations)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([name, operation]) => {
        const fields = [
          `method: ${JSON.stringify(operation.method)}`,
          `path: ${JSON.stringify(operation.path)}`,
          ...(["params", "query", "body"] as const).flatMap((key) =>
            operation[key] ? [`${key}: ${schemaSource(operation[key])}`] : [],
          ),
          `responses: { ${Object.entries(operation.responses)
            .sort(([a], [b]) => Number(a) - Number(b))
            .map(
              ([status, schema]) => `${status}: ${schema === null ? "null" : schemaSource(schema)}`,
            )
            .join(", ")} }`,
        ];
        return `    [${JSON.stringify(name)}]: { ${fields.join(", ")} },`;
      });
    return `  [${JSON.stringify(service.name)}]: defineService(${JSON.stringify(service.name)}, {\n${operations.join("\n")}\n  }),`;
  });
  return [
    "// Generated HTTP contracts. This module has no app implementation imports.",
    ...(sorted.some((service) =>
      Object.values(service.operations).some(
        (operation) =>
          operation.params ||
          operation.query ||
          operation.body ||
          Object.values(operation.responses).some((schema) => schema !== null),
      ),
    )
      ? ['import { Type as t } from "@sinclair/typebox";']
      : []),
    `import { ${sorted.length ? "defineService, " : ""}createServiceClient, type ServiceClient, type ServiceClientOptions } from "tako.sh/services";`,
    "",
    "export const services = {",
    ...declarations,
    "};",
    "export type ServiceContracts = typeof services;",
    "export function createClient<Name extends keyof ServiceContracts>(name: Name, options: ServiceClientOptions): ServiceClient<ServiceContracts[Name]> {",
    "  return createServiceClient(services[name], options);",
    "}",
    "",
  ].join("\n");
}
