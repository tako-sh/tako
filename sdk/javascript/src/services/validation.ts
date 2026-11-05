import { Kind, TypeRegistry, type TNumber, type TString, type TSchema } from "@sinclair/typebox";

const scalarKind = "TakoHttpJsonScalar";
const scalarValidator = Symbol.for("tako.http.scalar-validator");
type ScalarSchema = TSchema & { [scalarValidator]: (value: unknown) => boolean };
const schemas = new WeakMap<TSchema, { json: string; check: TSchema }>();

TypeRegistry.Set(scalarKind, (schema, value) => (schema as ScalarSchema)[scalarValidator](value));

/** The decimal spelling JSON emits, not the underlying IEEE-754 binary fraction. */
function decimal(value: number): { coefficient: bigint; exponent: number } {
  const [mantissa, power = "0"] = value.toString().split("e");
  const [whole, fraction = ""] = mantissa!.split(".");
  const digits = whole! + fraction;
  const trailing = digits.match(/0+$/)?.[0].length ?? 0;
  return {
    coefficient: BigInt(trailing ? digits.slice(0, -trailing) || "0" : digits),
    exponent: Number(power) - fraction.length + trailing,
  };
}

function isMultiple(value: number, divisor: number): boolean {
  const a = decimal(value);
  const b = decimal(divisor);
  const shift = a.exponent - b.exponent;
  // Finite JS number spellings bound the powers of ten; no quotient tolerance.
  return shift >= 0
    ? (a.coefficient * 10n ** BigInt(shift)) % b.coefficient === 0n
    : a.coefficient % (b.coefficient * 10n ** BigInt(-shift)) === 0n;
}

function scalarCheck(schema: TSchema): (value: unknown) => boolean {
  if (schema[Kind] === "String") {
    const { minLength, maxLength, pattern } = schema as TString;
    const regex = pattern === undefined ? undefined : new RegExp(pattern, "u");
    return (value) => {
      if (typeof value !== "string") return false;
      const length = Array.from(value).length;
      return (
        (minLength === undefined || length >= minLength) &&
        (maxLength === undefined || length <= maxLength) &&
        (regex === undefined || regex.test(value))
      );
    };
  }
  const { minimum, maximum, exclusiveMinimum, exclusiveMaximum, multipleOf } = schema as TNumber;
  const integer = schema[Kind] === "Integer";
  return (value) =>
    typeof value === "number" &&
    Number.isFinite(value) &&
    (!integer || Number.isInteger(value)) &&
    (minimum === undefined || value >= minimum) &&
    (maximum === undefined || value <= maximum) &&
    (exclusiveMinimum === undefined || value > exclusiveMinimum) &&
    (exclusiveMaximum === undefined || value < exclusiveMaximum) &&
    (multipleOf === undefined || isMultiple(value, multipleOf));
}

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
    return { ...schema, [Kind]: scalarKind, [scalarValidator]: scalarCheck(schema) };
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
