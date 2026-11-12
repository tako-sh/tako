/**
 * Channel params select the channel stream. Subscribers send them as flat
 * query pairs, and tako-server coerces each value by the type the params
 * schema declares for that property (`integer`, `number`, `boolean`;
 * everything else stays a string). A server-side publish must name the same
 * stream, so it sends the values a subscriber's query would be coerced to.
 */

/** Scalar value a channel param can carry. */
export type ChannelParamValue = string | number | boolean;

function invalidParam(key: string, reason: string): Error {
  return new TypeError(
    `Channel param '${key}' ${reason}. Channel params must be flat string, number, or boolean values.`,
  );
}

/**
 * Serialize params into the query pairs subscribe and connect send.
 * `undefined` and `null` values are omitted.
 *
 * @throws {TypeError} If a value is an object, array, or another non-scalar.
 */
export function channelParamQuery(params: Record<string, unknown>): Array<[string, string]> {
  const pairs: Array<[string, string]> = [];
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null) continue;
    if (typeof value === "string") {
      pairs.push([key, value]);
    } else if (typeof value === "number") {
      if (!Number.isFinite(value)) throw invalidParam(key, "is not a finite number");
      pairs.push([key, value.toString()]);
    } else if (typeof value === "boolean" || typeof value === "bigint") {
      pairs.push([key, value.toString()]);
    } else {
      throw invalidParam(key, Array.isArray(value) ? "is an array" : `has type ${typeof value}`);
    }
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
    case "integer":
      return INTEGER.test(raw) ? Number(raw) : raw;
    case "number":
      return NUMBER.test(raw) ? Number(raw) : raw;
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
 * @throws {TypeError} If a value is not a flat scalar.
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
      if (!Number.isSafeInteger(Number(value))) throw invalidParam(key, "is not a safe integer");
      bound[key] = Number(value);
    } else {
      bound[key] = value as ChannelParamValue;
    }
  }
  return bound;
}

/**
 * Reject params schemas whose properties cannot travel as query values.
 *
 * @throws {TypeError} If a declared property type is `object` or `array`.
 */
export function assertFlatChannelParamsSchema(channel: string, schema: object): void {
  const properties = (schema as { properties?: unknown }).properties;
  if (!properties || typeof properties !== "object") return;
  for (const key of Object.keys(properties)) {
    const type = declaredType(schema, key);
    const types = Array.isArray(type) ? type : [type];
    if (types.includes("object") || types.includes("array")) {
      throw new TypeError(
        `Channel '${channel}' param '${key}' is declared as ${String(type)}. Channel params must be flat string, number, or boolean values.`,
      );
    }
  }
}
