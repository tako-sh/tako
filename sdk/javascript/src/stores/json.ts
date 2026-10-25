/** Values returned by a successfully serialized Store read. */
export type StoreJsonValue =
  | null
  | boolean
  | number
  | string
  | StoreJsonValue[]
  | { [key: string]: StoreJsonValue };

declare const omitted: unique symbol;
type Omitted = typeof omitted;
type NonJsonValue =
  | undefined
  | symbol
  | ((...args: never[]) => unknown)
  | (abstract new (...args: never[]) => unknown);
type IsAny<T> = 0 extends 1 & T ? true : false;

type Serialized<T> =
  IsAny<T> extends true
    ? StoreJsonValue
    : unknown extends T
      ? StoreJsonValue
      : T extends { toJSON(key: string): infer Result }
        ? SerializedValue<Result>
        : SerializedValue<T>;

// JSON.stringify calls toJSON once for a value, then serializes the returned
// value directly. Its children's toJSON methods are still called normally.
type SerializedValue<T> =
  IsAny<T> extends true
    ? StoreJsonValue
    : unknown extends T
      ? StoreJsonValue
      : T extends null | boolean | number | string
        ? T
        : T extends bigint
          ? never
          : T extends NonJsonValue
            ? Omitted
            : T extends Number
              ? number
              : T extends String
                ? string
                : T extends Boolean
                  ? boolean
                  : T extends
                        | ReadonlyMap<unknown, unknown>
                        | ReadonlySet<unknown>
                        | WeakMap<object, unknown>
                        | WeakSet<object>
                        | RegExp
                        | Error
                    ? Record<string, never>
                    : T extends readonly unknown[]
                      ? { -readonly [Key in keyof T]: ArrayValue<T[Key]> }
                      : T extends object
                        ? SerializedObject<T>
                        : never;

type ArrayValue<T> =
  Serialized<T> extends infer Result ? (Result extends Omitted ? null : Result) : never;

type ObjectKey<T, Key extends keyof T> = Key extends string | number
  ? [Serialized<T[Key]>] extends [never]
    ? Key
    : [Serialized<T[Key]>] extends [Omitted]
      ? never
      : Key
  : never;

type Simplify<T> = { [Key in keyof T]: T[Key] };

type SerializedObject<T extends object> = Simplify<
  {
    [
      Key in keyof T as ObjectKey<T, Key> extends never
        ? never
        : Omitted extends Serialized<T[Key]>
          ? never
          : Key
    ]: Exclude<Serialized<T[Key]>, Omitted>;
  } & {
    [
      Key in keyof T as ObjectKey<T, Key> extends never
        ? never
        : Omitted extends Serialized<T[Key]>
          ? Key
          : never
    ]?: Exclude<Serialized<T[Key]>, Omitted>;
  }
>;

/**
 * The JSON representation of a server-side value. Dates and custom toJSON
 * results are converted, unsupported object members disappear, and unsupported
 * array entries become null. BigInt and cyclic values fail at runtime.
 *
 * For custom classes, declared data properties should be enumerable, or the
 * class should supply toJSON. Getters and property enumerability are not
 * represented by TypeScript types.
 */
export type Jsonify<T> = Exclude<Serialized<T>, Omitted>;

/** Keep platform JSON semantics while rejecting silently lossy numeric data. */
export function stringifyStoreJson(value: unknown): string | undefined {
  return JSON.stringify(value, (_key, value: unknown) => {
    if ((typeof value === "number" || value instanceof Number) && !Number.isFinite(Number(value))) {
      throw new Error("Store values must contain finite JSON numbers.");
    }
    return value;
  });
}

export function stringifyStoreSnapshot(snapshot: { data: unknown; offline?: unknown }): string {
  const encoded = stringifyStoreJson(snapshot);
  if (encoded === undefined || !Object.hasOwn(JSON.parse(encoded), "data")) {
    throw new Error("Store load must return JSON data; use null for an empty result.");
  }
  return encoded;
}
