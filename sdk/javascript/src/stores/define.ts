import type { Static, TSchema } from "@sinclair/typebox";
import { Type } from "@sinclair/typebox";
import { Value } from "@sinclair/typebox/value";
import { Channel } from "../channels";
import { defineChannel } from "../channels/define";
import type {
  ChannelAuthResult,
  ChannelDefinition,
  ChannelLifecycleConfig,
  VerifyInput,
} from "../channels/meta";
import type { ChannelHeaderValue } from "../types";
import { stringifyStoreJson, type Jsonify } from "./json";

export const STORE_CHANNEL_PREFIX = "tako.store:";
export const STORE_SYMBOL = Symbol.for("tako.store");

/** Authorization runs independently for each read and live subscription. */
export interface StoreVerifyInput<Params> {
  store: string;
  params: Params;
  header?: ChannelHeaderValue;
  cookie?: string;
}

export interface StoreLoadInput<Params> {
  params: Params;
  /** Identity returned by the verifier; never taken from client params. */
  subject?: string;
  signal: AbortSignal;
}

/** Allow clients to opt into persisted offline reads with a non-secret cache scope. */
export interface StoreOfflineConfig {
  maxAgeMs: number;
  /** Bump when the data shape changes incompatibly. @defaultValue "1" */
  version?: string;
}

export interface StoreConfig<Params, Data> extends Pick<
  ChannelLifecycleConfig,
  "keepaliveIntervalMs" | "maxConnectionLifetimeMs"
> {
  auth?:
    | false
    | {
        headerName?: string | false;
        cookieName?: string;
        verify(input: StoreVerifyInput<Params>): ChannelAuthResult | Promise<ChannelAuthResult>;
      };
  load(input: StoreLoadInput<Params>): Data | Promise<Data>;
  offline?: StoreOfflineConfig;
}

/** Server-side definition discovered from `<app_root>/stores/`. */
export interface StoreDefinition<Params = Record<string, never>, Data = unknown> {
  readonly type: typeof STORE_SYMBOL;
  readonly name: string;
  readonly definition: ChannelDefinition<Params>;
  readonly offline?: Readonly<{ maxAgeMs: number; version: string }>;
  readonly load: StoreConfig<Params, Data>["load"];
  readonly __params?: Params;
  readonly __data?: Jsonify<Data>;
  /** Call after the database transaction commits. Publication failures reject. */
  invalidate(
    ...args: Record<string, never> extends Params ? [params?: Params] : [params: Params]
  ): Promise<void>;
}

export function defineStore<S extends TSchema, Data>(
  name: string,
  config: StoreConfig<Static<S>, Data> & { paramsSchema: (t: typeof Type) => S },
): StoreDefinition<Static<S>, Awaited<Data>>;
export function defineStore<Data>(
  name: string,
  config: StoreConfig<Record<string, never>, Data> & { paramsSchema?: undefined },
): StoreDefinition<Record<string, never>, Awaited<Data>>;
export function defineStore(name: string, input: unknown): unknown {
  const config = input as StoreConfig<unknown, unknown> & {
    paramsSchema?: (t: typeof Type) => TSchema;
  };
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,255}$/.test(name)) {
    throw new Error(
      "Store names must contain 1–256 ASCII letters, digits, dots, underscores, or hyphens and begin with a letter or digit.",
    );
  }
  if (typeof config.load !== "function")
    throw new Error(`Store '${name}' requires a load function.`);
  for (const value of [config.keepaliveIntervalMs, config.maxConnectionLifetimeMs]) {
    if (value !== undefined && (!Number.isSafeInteger(value) || value <= 0)) {
      throw new Error("Store connection intervals must be positive safe integers.");
    }
  }
  const offline =
    config.offline &&
    Object.freeze({
      maxAgeMs: config.offline.maxAgeMs,
      version: config.offline.version ?? "1",
    });
  if (
    offline &&
    (!Number.isSafeInteger(offline.maxAgeMs) ||
      offline.maxAgeMs <= 0 ||
      typeof offline.version !== "string" ||
      !offline.version ||
      new TextEncoder().encode(offline.version).length > 128)
  ) {
    throw new Error(
      "Store offline policy requires a positive integer maxAgeMs and a nonempty version.",
    );
  }
  const schema = config.paramsSchema?.(Type) ?? Type.Object({});
  const channel = defineChannel(`${STORE_CHANNEL_PREFIX}${name}`, {
    paramsSchema: () => schema,
    ...(config.keepaliveIntervalMs !== undefined && {
      keepaliveIntervalMs: config.keepaliveIntervalMs,
    }),
    ...(config.maxConnectionLifetimeMs !== undefined && {
      maxConnectionLifetimeMs: config.maxConnectionLifetimeMs,
    }),
    ...(config.auth && {
      auth: {
        ...config.auth,
        verify: (input: VerifyInput<unknown>) =>
          config.auth && input.operation === "subscribe"
            ? config.auth.verify({
                store: name,
                params: input.params,
                ...(input.header !== undefined && { header: input.header }),
                ...(input.cookie !== undefined && { cookie: input.cookie }),
              })
            : false,
      },
    }),
  });
  const definition = channel.definition;
  return Object.freeze({
    type: STORE_SYMBOL,
    name,
    definition,
    load: config.load,
    ...(offline && { offline }),
    async invalidate(params: unknown = {}): Promise<void> {
      // Snapshot and validate before sending: callers can mutate their objects
      // while the RPC is in flight, and unsupported JSON must fail visibly.
      const encoded = stringifyStoreJson(params);
      if (encoded === undefined || new TextEncoder().encode(encoded).length > 16 * 1024) {
        throw new Error("Store invalidation params must be JSON and at most 16 KiB.");
      }
      const scopeParams: unknown = JSON.parse(encoded);
      if (
        !scopeParams ||
        typeof scopeParams !== "object" ||
        Array.isArray(scopeParams) ||
        !Value.Check(definition.paramsSchema as TSchema, scopeParams)
      ) {
        throw new Error(`Invalid params for store '${name}'.`);
      }
      await new Channel(definition.channel).publish({
        type: "tako.store.invalidate",
        data: {},
        // Trusted socket metadata, consumed before the message is persisted.
        ...{ scopeParams },
      });
    },
  });
}

export function isStoreDefinition(value: unknown): value is StoreDefinition<unknown, unknown> {
  return !!value && typeof value === "object" && (value as StoreDefinition).type === STORE_SYMBOL;
}
