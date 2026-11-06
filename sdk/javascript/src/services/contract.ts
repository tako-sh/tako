import {
  Kind,
  OptionalKind,
  TransformKind,
  TypeGuard,
  type Static,
  type TObject,
  type TSchema,
} from "@sinclair/typebox";

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS";

export interface ServiceOperation {
  readonly method: HttpMethod;
  readonly path: string;
  readonly params?: TObject;
  readonly query?: TObject;
  readonly body?: TSchema;
  /** Use null for a response with no body. */
  readonly responses: Readonly<Record<number, TSchema | null>>;
}

export interface ServiceDefinition<
  Operations extends Record<string, ServiceOperation> = Record<string, ServiceOperation>,
> {
  readonly name: string;
  readonly operations: Operations;
}

type InputField<O, K extends "params" | "query" | "body"> =
  O extends Record<K, infer S extends TSchema>
    ? S extends { [OptionalKind]: "Optional" }
      ? { [P in K]?: Static<S> }
      : K extends "query"
        ? {} extends Static<S>
          ? { [P in K]?: Static<S> }
          : { [P in K]: Static<S> }
        : { [P in K]: Static<S> }
    : { [P in K]?: never };

export type ServiceInput<O extends ServiceOperation> = InputField<O, "params"> &
  InputField<O, "query"> &
  InputField<O, "body">;
type ResponseStatus<S> = S extends number ? S : S extends `${infer N extends number}` ? N : never;
export type ServiceResponse<O extends ServiceOperation> = {
  [S in keyof O["responses"] & (number | `${number}`)]: {
    status: ResponseStatus<S>;
    headers?: HeadersInit;
    body: O["responses"][S] extends TSchema ? Static<O["responses"][S]> : undefined;
  };
}[keyof O["responses"] & (number | `${number}`)];

const methods = new Set(["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]);
const schemaKeywords: Record<string, readonly string[]> = {
  String: ["type", "minLength", "maxLength", "pattern"],
  Number: ["type", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf"],
  Integer: ["type", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf"],
  Boolean: ["type"],
  Null: ["type"],
  Literal: ["type", "const"],
  Array: ["type", "items", "minItems", "maxItems", "uniqueItems"],
  Object: [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "minProperties",
    "maxProperties",
  ],
  Union: ["anyOf"],
  Intersect: ["type", "allOf"],
};
const annotations = new Set([
  "title",
  "description",
  "default",
  "examples",
  "$id",
  "$schema",
  "$comment",
  "deprecated",
  "readOnly",
  "writeOnly",
]);

/** Checks the portable schema subset supported by service payload validation. */
export function assertServiceSchema(schema: TSchema): void {
  if (
    !TypeGuard.IsSchema(schema) ||
    !Object.hasOwn(schemaKeywords, schema[Kind]) ||
    TransformKind in schema ||
    "$ref" in schema ||
    "format" in schema
  ) {
    throw new Error(
      `Unsupported service schema '${schema[Kind]}'. Use inline JSON schemas without transforms, references, or formats.`,
    );
  }
  for (const key of Object.keys(schema)) {
    if (!annotations.has(key) && !schemaKeywords[schema[Kind]]!.includes(key))
      throw new Error(`Unsupported service schema keyword '${key}'.`);
  }
  if (schema["multipleOf"] !== undefined && !(schema["multipleOf"] > 0))
    throw new Error("Service schema multipleOf must be positive.");
  if (schema["pattern"] !== undefined) new RegExp(schema["pattern"] as string, "u");
  if (schema[Kind] === "Object") {
    const properties = schema["properties"] as Record<string, TSchema>;
    const expected = Object.keys(properties).filter((key) => !properties[key]![OptionalKind]);
    const required = (schema["required"] ?? []) as string[];
    // Generation and Static derive presence from OptionalKind, so metadata must agree.
    if (
      required.length !== expected.length ||
      new Set(required).size !== required.length ||
      expected.some((key) => !required.includes(key))
    )
      throw new Error("Service schema required must match declared non-optional properties.");
    for (const child of Object.values(schema["properties"] ?? {}))
      assertServiceSchema(child as TSchema);
    if (schema["additionalProperties"] && typeof schema["additionalProperties"] === "object")
      assertServiceSchema(schema["additionalProperties"] as TSchema);
  }
  if (schema["type"] === "array") assertServiceSchema(schema["items"] as TSchema);
  for (const key of ["anyOf", "allOf"] as const) {
    for (const child of (schema[key] ?? []) as TSchema[]) assertServiceSchema(child);
  }
  JSON.stringify(schema, (_key, value: unknown) => {
    if (
      typeof value === "function" ||
      typeof value === "bigint" ||
      typeof value === "symbol" ||
      value === undefined ||
      (typeof value === "number" && !Number.isFinite(value))
    ) {
      throw new Error("Service schemas must be serializable JSON.");
    }
    return value;
  });
}

function assertParameters(schema: TObject | undefined, path: boolean): void {
  if (!schema) return;
  if (schema[Kind] !== "Object" || schema[OptionalKind])
    throw new Error("HTTP params and query must be object schemas.");
  if (schema["additionalProperties"])
    throw new Error("HTTP parameters cannot declare additional properties.");
  for (const field of Object.values(schema["properties"])) {
    const scalar = !path && field["type"] === "array" ? (field["items"] as TSchema) : field;
    if (
      !(path ? ["string"] : ["string", "number", "integer", "boolean"]).includes(
        scalar["type"] as string,
      )
    ) {
      throw new Error(
        "Path params must be strings; query fields must be scalars or arrays of scalars.",
      );
    }
    if (path && field[OptionalKind]) throw new Error("Path params must be required.");
  }
}

export function defineService<const Operations extends Record<string, ServiceOperation>>(
  name: string,
  operations: Operations,
): ServiceDefinition<Operations> {
  if (!name.trim()) throw new Error("Service name must not be empty.");
  const routes = new Set<string>();
  for (const [id, operation] of Object.entries(operations)) {
    if (!id.trim()) throw new Error("Operation name must not be empty.");
    if (!methods.has(operation.method)) throw new Error(`Invalid HTTP method for '${id}'.`);
    if (
      !/^\/(?:[A-Za-z0-9._~-]+|\{[A-Za-z_][A-Za-z0-9_]*\})?(?:\/(?:[A-Za-z0-9._~-]+|\{[A-Za-z_][A-Za-z0-9_]*\}))*\/?$/.test(
        operation.path,
      ) ||
      operation.path.includes("//") ||
      operation.path.split("/").some((part) => part === "." || part === "..")
    ) {
      throw new Error(`Invalid HTTP path for '${id}'. Use /items/{id}.`);
    }
    const route = `${operation.method} ${operation.path.replace(/\{[^}]+\}/g, "{}")}`;
    if (routes.has(route)) throw new Error(`Duplicate HTTP route '${route}'.`);
    routes.add(route);
    const params = [...operation.path.matchAll(/\{([^}]+)\}/g)].map((match) => match[1]!);
    const keys = Object.keys(operation.params?.["properties"] ?? {});
    if (
      new Set(params).size !== params.length ||
      params.length !== keys.length ||
      params.some((key) => !keys.includes(key))
    ) {
      throw new Error(`Path placeholders and params schema must match for '${id}'.`);
    }
    assertParameters(operation.params, true);
    assertParameters(operation.query, false);
    if ((operation.method === "GET" || operation.method === "HEAD") && operation.body)
      throw new Error(`${operation.method} cannot have a request body.`);
    if (!Object.keys(operation.responses).length)
      throw new Error(`Operation '${id}' needs a response.`);
    for (const [status, schema] of Object.entries(operation.responses)) {
      if (!/^[2-5][0-9]{2}$/.test(status)) throw new Error(`Invalid response status '${status}'.`);
      if (
        ([204, 205, 304].includes(Number(status)) || operation.method === "HEAD") &&
        schema !== null
      )
        throw new Error(`Status ${status} or HEAD requires an empty response.`);
      if (schema !== null) {
        if (schema[OptionalKind])
          throw new Error("Use null for empty responses, not an optional response schema.");
        assertServiceSchema(schema);
      }
    }
    for (const schema of [operation.params, operation.query, operation.body])
      if (schema) assertServiceSchema(schema);
  }
  const definitions = Object.fromEntries(
    Object.entries(operations).map(([id, operation]) => [
      id,
      {
        ...operation,
        ...(operation.params
          ? { params: { ...operation.params, additionalProperties: false } }
          : {}),
        ...(operation.query ? { query: { ...operation.query, additionalProperties: false } } : {}),
      },
    ]),
  );
  return { name, operations: definitions as Operations };
}
