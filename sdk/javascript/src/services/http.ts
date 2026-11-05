import { OptionalKind, Type, type TObject, type TSchema } from "@sinclair/typebox";
import { Value } from "@sinclair/typebox/value";

export type ServiceErrorCode =
  | "validation"
  | "unexpected-status"
  | "content-type"
  | "invalid-json"
  | "transport";
export type ServiceErrorPhase = "request" | "response";
export interface ServiceValidationIssue {
  path: string;
  message: string;
}

export class ServiceError extends Error {
  override readonly name = "ServiceError";
  constructor(
    readonly code: ServiceErrorCode,
    readonly operation: string,
    readonly phase: ServiceErrorPhase,
    message: string,
    readonly status?: number,
    readonly issues: readonly ServiceValidationIssue[] = [],
    options?: ErrorOptions,
  ) {
    super(`${operation}: ${message}`, options);
  }
}

const jsonSchema = Type.Recursive((self) =>
  Type.Union([
    Type.Null(),
    Type.Boolean(),
    Type.Number(),
    Type.String(),
    Type.Array(self),
    Type.Record(Type.String(), self),
  ]),
);

export function validate(
  schema: TSchema | undefined,
  value: unknown,
  operation: string,
  phase: ServiceErrorPhase,
  field: string,
  status?: number,
): void {
  if (
    schema
      ? (value === undefined && schema[OptionalKind]) || Value.Check(schema, value)
      : value === undefined
  )
    return;
  const issues = schema
    ? [...Value.Errors(schema, value)].map(({ path, message }) => ({
        path: `/${field}${path}`,
        message,
      }))
    : [{ path: `/${field}`, message: "Not declared in the contract" }];
  throw new ServiceError("validation", operation, phase, `Invalid ${field}`, status, issues);
}

export function jsonBody(
  value: unknown,
  operation: string,
  phase: ServiceErrorPhase,
  status?: number,
): string {
  try {
    if (Value.Check(jsonSchema, value)) return JSON.stringify(value);
  } catch (cause) {
    throw new ServiceError(
      "validation",
      operation,
      phase,
      "Body must be serializable JSON",
      status,
      [],
      { cause },
    );
  }
  throw new ServiceError(
    "validation",
    operation,
    phase,
    "Body must contain only JSON values",
    status,
  );
}

export async function readText(
  message: Request | Response,
  operation: string,
  phase: ServiceErrorPhase,
  status?: number,
): Promise<string> {
  try {
    return await message.text();
  } catch (cause) {
    throw new ServiceError("transport", operation, phase, "Failed to read HTTP body", status, [], {
      cause,
    });
  }
}

export async function readJson(
  message: Request | Response,
  operation: string,
  phase: ServiceErrorPhase,
  status?: number,
): Promise<unknown> {
  const text = await readText(message, operation, phase, status);
  if (!text.length) return undefined;
  const contentType = message.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
  if (
    contentType !== "application/json" &&
    !/^application\/[a-z0-9!#$&^_.+-]+\+json$/.test(contentType ?? "")
  ) {
    throw new ServiceError("content-type", operation, phase, "Expected application/json", status);
  }
  try {
    return JSON.parse(text) as unknown;
  } catch (cause) {
    throw new ServiceError("invalid-json", operation, phase, "Malformed JSON body", status, [], {
      cause,
    });
  }
}

export function decodeQuery(
  schema: TObject | undefined,
  search: URLSearchParams,
): Record<string, unknown> | undefined {
  if (!schema) return search.size ? Object.fromEntries(search) : undefined;
  const query: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
  const scalar = (field: TSchema, text: string): unknown => {
    if (
      (field["type"] === "number" || field["type"] === "integer") &&
      /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?$/.test(text)
    )
      return Number(text);
    if (field["type"] === "boolean" && (text === "true" || text === "false"))
      return text === "true";
    return text;
  };
  for (const key of new Set(search.keys())) {
    const field = schema["properties"][key];
    const values = search.getAll(key);
    query[key] =
      field?.["type"] === "array"
        ? values.map((value) => scalar(field["items"] as TSchema, value))
        : values.length > 1
          ? values
          : field
            ? scalar(field, values[0]!)
            : values[0];
  }
  return query;
}
