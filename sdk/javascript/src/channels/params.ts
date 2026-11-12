/**
 * Channel params select the channel stream. Subscribers send them as flat
 * query pairs, and tako-server coerces each value by the type the params
 * schema declares for that property (`integer`, `number`, `boolean`;
 * everything else stays a string). A server-side publish must name the same
 * stream, so it sends the values a subscriber's query would be coerced to.
 *
 * A value that a JavaScript number cannot carry exactly would name one stream
 * when subscribing and another when publishing, so binding rejects it: an
 * `integer` param must be a safe integer (at most 2^53 - 1 in magnitude) and a
 * `number` param must be finite. Large identifiers such as snowflake ids
 * belong in string params.
 *
 * `from` and `last_message_id` are reserved query names (the replay start and
 * the WebSocket replay cursor), so no channel param may use them.
 */

/** Query names tako-server reserves on channel routes; channel params cannot use them. */
export const RESERVED_CHANNEL_PARAM_NAMES: readonly string[] = ["from", "last_message_id"];

function reservedParam(key: string): TypeError {
  return new TypeError(
    `Channel param '${key}' uses a reserved name. Tako reserves ${RESERVED_CHANNEL_PARAM_NAMES.map((name) => `'${name}'`).join(" and ")} on channel routes; rename the param.`,
  );
}

/** Scalar value a channel param can carry. */
export type ChannelParamValue = string | number | boolean;

function invalidParam(key: string, reason: string): Error {
  return new TypeError(
    `Channel param '${key}' ${reason}. Channel params must be flat string, number, or boolean values.`,
  );
}

function unsafeInteger(key: string, raw: string): Error {
  return new TypeError(
    `Channel param '${key}' is ${raw}, outside the safe integer range (at most ${Number.MAX_SAFE_INTEGER} in magnitude), so publish and subscribe could select different streams. Declare large identifiers such as snowflake ids as strings (t.String()).`,
  );
}

/**
 * Serialize params into the query pairs subscribe and connect send.
 * `undefined` and `null` values are omitted. With a schema, each value is
 * checked as {@link channelStreamParams} checks it, so a binding fails the
 * same way whether it subscribes or publishes.
 *
 * @throws {TypeError} If a value is an object, array, or another non-scalar,
 * an unsafe integer or non-finite number, or a key is a reserved name
 * (`from`, `last_message_id`).
 */
export function channelParamQuery(
  params: Record<string, unknown>,
  schema?: object,
): Array<[string, string]> {
  const pairs: Array<[string, string]> = [];
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null) continue;
    if (RESERVED_CHANNEL_PARAM_NAMES.includes(key)) throw reservedParam(key);
    if (typeof value === "string") {
      pairs.push([key, value]);
    } else if (typeof value === "number") {
      if (!Number.isFinite(value)) throw invalidParam(key, "is not a finite number");
      pairs.push([key, value.toString()]);
    } else if (typeof value === "bigint") {
      if (!Number.isSafeInteger(Number(value))) throw unsafeInteger(key, value.toString());
      pairs.push([key, value.toString()]);
    } else if (typeof value === "boolean") {
      pairs.push([key, value.toString()]);
    } else {
      throw invalidParam(key, Array.isArray(value) ? "is an array" : `has type ${typeof value}`);
    }
  }
  if (schema !== undefined) {
    for (const [key, raw] of pairs) coerce(schema, key, raw);
  }
  return pairs;
}

const INTEGER = /^[+-]?\d+$/;
const NUMBER = /^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$/;

function declaredType(schema: unknown, key: string): unknown {
  if (!schema || typeof schema !== "object") return undefined;
  const properties = (schema as { properties?: unknown }).properties;
  if (!properties || typeof properties !== "object") return undefined;
  const property = (properties as Record<string, unknown>)[key];
  if (!property || typeof property !== "object") return undefined;
  return (property as { type?: unknown }).type;
}

function coerce(schema: unknown, key: string, raw: string): ChannelParamValue {
  switch (declaredType(schema, key)) {
    case "integer": {
      if (!INTEGER.test(raw)) return raw;
      const value = Number(raw);
      if (!Number.isSafeInteger(value)) throw unsafeInteger(key, raw);
      return value;
    }
    case "number": {
      if (!NUMBER.test(raw)) return raw;
      const value = Number(raw);
      if (!Number.isFinite(value)) throw invalidParam(key, `is ${raw}, not a finite number`);
      return value;
    }
    case "boolean":
      return raw === "true" ? true : raw === "false" ? false : raw;
    default:
      return raw;
  }
}

/**
 * The params a server-side publish sends: the values tako-server binds for a
 * subscriber whose query carries the same params. With a schema, each value
 * is serialized as a query value and coerced by the schema's declared
 * property type, so `{ page: 2 }` and `{ page: "2" }` name the same stream
 * when `page` is an integer. Without a schema, scalars are sent as given.
 *
 * @throws {TypeError} If a value is not a flat scalar, an `integer` param is
 * not a safe integer, or a `number` param is not finite.
 */
export function channelStreamParams(
  params: Record<string, unknown>,
  schema?: object,
): Record<string, ChannelParamValue> {
  const bound: Record<string, ChannelParamValue> = {};
  for (const [key, raw] of channelParamQuery(params)) {
    if (schema !== undefined) {
      bound[key] = coerce(schema, key, raw);
      continue;
    }
    const value = params[key];
    if (typeof value === "bigint") {
      bound[key] = Number(value);
    } else {
      bound[key] = value as ChannelParamValue;
    }
  }
  return bound;
}

/** Top-level keywords that place properties outside the schema's top-level `properties`. */
const NON_FLAT_SCHEMA_KEYWORDS = ["$ref", "$dynamicRef", "patternProperties"];

/** Combinators whose subschemas apply to the whole params object. */
const ARRAY_COMBINATORS = ["allOf", "anyOf", "oneOf"];
const SCHEMA_COMBINATORS = ["not", "if", "then", "else"];
const MAP_COMBINATORS = ["dependentSchemas"];

/** Keywords of a combinator subschema that declare params. */
const DECLARING_KEYWORDS = [
  "properties",
  "patternProperties",
  "additionalProperties",
  "$ref",
  "$dynamicRef",
];

function isSchemaObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The subschemas a schema's combinators apply to the same instance, with their combinator. */
function combinatorSubschemas(schema: Record<string, unknown>): Array<[string, unknown]> {
  const subschemas: Array<[string, unknown]> = [];
  for (const keyword of ARRAY_COMBINATORS) {
    const items = schema[keyword];
    if (Array.isArray(items)) for (const item of items) subschemas.push([keyword, item]);
  }
  for (const keyword of SCHEMA_COMBINATORS) {
    if (keyword in schema) subschemas.push([keyword, schema[keyword]]);
  }
  for (const keyword of MAP_COMBINATORS) {
    const entries = schema[keyword];
    if (isSchemaObject(entries)) {
      for (const subschema of Object.values(entries)) subschemas.push([keyword, subschema]);
    }
  }
  return subschemas;
}

/**
 * The keyword through which a combinator subschema, or one nested in it,
 * declares params. `additionalProperties: false` only constrains.
 */
function declaringKeyword(subschema: unknown): string | undefined {
  if (!isSchemaObject(subschema)) return undefined;
  const keyword = DECLARING_KEYWORDS.find(
    (name) => name in subschema && subschema[name] !== false && subschema[name] !== undefined,
  );
  if (keyword !== undefined) return keyword;
  for (const [, nested] of combinatorSubschemas(subschema)) {
    const found = declaringKeyword(nested);
    if (found !== undefined) return found;
  }
  return undefined;
}

function combinatorProblem(schema: Record<string, unknown>): string | undefined {
  for (const [combinator, subschema] of combinatorSubschemas(schema)) {
    const keyword = declaringKeyword(subschema);
    if (keyword !== undefined) {
      return `declares params through '${keyword}' inside '${combinator}' (combinators may only add constraints such as 'required' over the top-level 'properties')`;
    }
  }
  return undefined;
}

/**
 * Reject params schemas whose properties cannot travel as query values.
 *
 * Top-level combinators (`allOf`, `anyOf`, `oneOf`, `not`, `if`/`then`/`else`,
 * `dependentSchemas`) may add constraints such as `required` over the declared
 * params, but may not declare params themselves.
 *
 * @throws {TypeError} If the schema is not one object schema with its params
 * under top-level `properties` (for example `t.Intersect`, `t.Union`, or
 * `t.Ref`), a combinator declares `properties`, `patternProperties`,
 * `additionalProperties` (other than `false`) or a `$ref`, a declared property
 * type is `object` or `array`, or a property uses a reserved name (`from`,
 * `last_message_id`).
 */
export function assertFlatChannelParamsSchema(channel: string, schema: object): void {
  const { type: schemaType, properties } = schema as { type?: unknown; properties?: unknown };
  const keyword = NON_FLAT_SCHEMA_KEYWORDS.find((name) => name in schema);
  const problem =
    keyword !== undefined
      ? `uses '${keyword}'`
      : schemaType !== undefined && schemaType !== "object"
        ? `has type ${JSON.stringify(schemaType)}`
        : properties !== undefined && (typeof properties !== "object" || properties === null)
          ? "has 'properties' that is not an object"
          : combinatorProblem(schema as Record<string, unknown>);
  if (problem !== undefined) {
    throw new TypeError(
      `Channel '${channel}' params schema ${problem}. Params must be declared under the top-level 'properties' of one object schema: use a single t.Object({...}), and combine object schemas with t.Composite([...]).`,
    );
  }
  if (!properties || typeof properties !== "object") return;
  for (const key of Object.keys(properties)) {
    if (RESERVED_CHANNEL_PARAM_NAMES.includes(key)) {
      throw new TypeError(
        `Channel '${channel}' declares param '${key}', a name Tako reserves on channel routes (${RESERVED_CHANNEL_PARAM_NAMES.map((name) => `'${name}'`).join(", ")}). Rename the param.`,
      );
    }
    const type = declaredType(schema, key);
    const types = Array.isArray(type) ? type : [type];
    if (types.includes("object") || types.includes("array")) {
      throw new TypeError(
        `Channel '${channel}' param '${key}' is declared as ${String(type)}. Channel params must be flat string, number, or boolean values.`,
      );
    }
  }
}
