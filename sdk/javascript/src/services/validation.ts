import { Kind, TypeRegistry, type TSchema } from "@sinclair/typebox";
import Ajv, { type ValidateFunction } from "ajv";

const scalarKind = "TakoHttpJsonScalar";
const scalarValidator = Symbol.for("tako.http.scalar-validator");
type ScalarSchema = TSchema & { [scalarValidator]: ValidateFunction };
const schemas = new WeakMap<TSchema, { json: string; check: TSchema }>();
const validator = new Ajv({ strict: false, addUsedSchema: false, multipleOfPrecision: 12 });

TypeRegistry.Set(scalarKind, (schema, value) => (schema as ScalarSchema)[scalarValidator](value));

/** Keep structural validation in TypeBox while using JSON Schema scalar semantics. */
export function serviceValidationSchema(schema: TSchema): TSchema {
  const json = JSON.stringify(schema);
  const cached = schemas.get(schema);
  if (cached?.json === json) return cached.check;
  const check = compileSchema(schema);
  schemas.set(schema, { json, check });
  return check;
}

function compileSchema(schema: TSchema): TSchema {
  if (["String", "Number", "Integer"].includes(schema[Kind])) {
    const jsonSchema = Object.fromEntries(
      Object.entries(schema).filter(([key]) => !key.startsWith("$")),
    );
    const check = validator.compile(jsonSchema);
    validator.removeSchema(jsonSchema);
    return { ...schema, [Kind]: scalarKind, [scalarValidator]: check };
  }
  return {
    ...schema,
    ...(schema["properties"]
      ? {
          properties: Object.fromEntries(
            Object.entries(schema["properties"] as Record<string, TSchema>).map(([key, field]) => [
              key,
              serviceValidationSchema(field),
            ]),
          ),
        }
      : {}),
    ...(schema["items"] ? { items: serviceValidationSchema(schema["items"] as TSchema) } : {}),
    ...(typeof schema["additionalProperties"] === "object"
      ? { additionalProperties: serviceValidationSchema(schema["additionalProperties"] as TSchema) }
      : {}),
    ...(schema["anyOf"]
      ? { anyOf: (schema["anyOf"] as TSchema[]).map(serviceValidationSchema) }
      : {}),
    ...(schema["allOf"]
      ? { allOf: (schema["allOf"] as TSchema[]).map(serviceValidationSchema) }
      : {}),
  };
}
