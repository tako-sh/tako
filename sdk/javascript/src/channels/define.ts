import { Type, type Static, type TSchema } from "@sinclair/typebox";
import { Channel } from "../channels";
import {
  bindChannelName,
  CHANNEL_SYMBOL,
  isChannelDefinition,
  isChannelExport as isChannelExportMeta,
  type ChannelAuthConfig,
  type ChannelAuthResult,
  type ChannelAuthScheme,
  type ChannelDefinition,
  type ChannelHandlerContext,
  type ChannelLifecycleConfig,
  type MessageHandler,
  type VerifyInput,
} from "./meta";
import type {
  ChannelConnectOptions,
  ChannelMessage,
  ChannelPublishOptions,
  ChannelSocket,
  ChannelSubscribeOptions,
  ChannelSubscription,
} from "../types";

export interface ChannelConfig<
  ParamsSchema extends TSchema | undefined,
  Params,
  Messages,
> extends ChannelLifecycleConfig {
  paramsSchema?: (t: typeof Type) => ParamsSchema extends TSchema ? ParamsSchema : TSchema;
  auth?: false | ChannelAuthConfig<Params>;
  handler?: { [T in keyof Messages]?: MessageHandler<Messages[T], Params> };
}

export interface ChannelHandle<Params, Messages> {
  readonly __params?: Params;
  readonly name: string;
  publish<T extends keyof Messages & string>(
    message: { type: T; data: Messages[T] },
    options?: ChannelPublishOptions,
  ): Promise<ChannelMessage<Messages[T]>>;
  subscribe(options?: ChannelSubscribeOptions): ChannelSubscription;
  connect?(options?: ChannelConnectOptions): ChannelSocket;
}

export interface ChannelExportMeta<Params, Messages> {
  readonly definition: ChannelDefinition<Params, Messages>;
  $messageTypes<NewMessages>(): ChannelExport<Params, NewMessages>;
}

export type ChannelExport<Params, Messages> = (Record<string, never> extends Params
  ? ChannelHandle<Params, Messages>
  : (params: Params) => ChannelHandle<Params, Messages>) &
  ChannelExportMeta<Params, Messages>;

function lifecycle(config: ChannelLifecycleConfig): ChannelLifecycleConfig {
  return {
    ...(config.replayWindowMs !== undefined && { replayWindowMs: config.replayWindowMs }),
    ...(config.inactivityTtlMs !== undefined && { inactivityTtlMs: config.inactivityTtlMs }),
    ...(config.keepaliveIntervalMs !== undefined && {
      keepaliveIntervalMs: config.keepaliveIntervalMs,
    }),
    ...(config.maxConnectionLifetimeMs !== undefined && {
      maxConnectionLifetimeMs: config.maxConnectionLifetimeMs,
    }),
  };
}

function encodeParams(params: Record<string, unknown>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null) continue;
    search.set(key, encodeQueryValue(value));
  }
  const query = search.toString();
  return query ? `?${query}` : "";
}

function encodeQueryValue(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean" || typeof value === "bigint") {
    return value.toString();
  }
  return JSON.stringify(value);
}

function makeHandle<P, M>(definition: ChannelDefinition<P, M>, params: P): ChannelHandle<P, M> {
  const query = encodeParams(params as Record<string, unknown>);
  const makeChannel = () =>
    new Channel(definition.channel ?? "", definition.transport, params as Record<string, unknown>);
  const handle = {
    get name() {
      return `${definition.channel ?? ""}${query}`;
    },
    publish<T extends keyof M & string>(
      message: { type: T; data: M[T] },
      options?: ChannelPublishOptions,
    ) {
      return makeChannel().publish(message, options);
    },
    subscribe(options?: ChannelSubscribeOptions) {
      return makeChannel().subscribe(options);
    },
  };
  if (definition.transport === "ws") {
    Object.defineProperty(handle, "connect", {
      value(options?: ChannelConnectOptions) {
        return makeChannel().connect(options);
      },
      enumerable: true,
      configurable: true,
    });
  }
  return handle as ChannelHandle<P, M>;
}

function attachMeta<P, M, T extends object>(
  target: T,
  definition: ChannelDefinition<P, M>,
): T & ChannelExportMeta<P, M> {
  Object.defineProperty(target, "definition", {
    value: definition,
    writable: false,
    enumerable: false,
    configurable: false,
  });
  Object.defineProperty(target, "$messageTypes", {
    value: function messageTypesNarrow<NewM>() {
      return this as unknown as ChannelExport<P, NewM>;
    },
    writable: false,
    enumerable: false,
    configurable: false,
  });
  return target as T & ChannelExportMeta<P, M>;
}

export function defineChannel<
  ParamsSchema extends TSchema | undefined = undefined,
  Params = ParamsSchema extends TSchema ? Static<ParamsSchema> : Record<string, never>,
  Messages = Record<string, unknown>,
>(config: ChannelConfig<ParamsSchema, Params, Messages> = {}): ChannelExport<Params, Messages> {
  const schema = config.paramsSchema?.(Type) ?? Type.Object({});
  const auth: ChannelDefinition<Params, Messages>["auth"] =
    config.auth === undefined || config.auth === false
      ? false
      : {
          headerName:
            config.auth.headerName === undefined ? "authorization" : config.auth.headerName,
          ...(config.auth.cookieName !== undefined && { cookieName: config.auth.cookieName }),
          verify: config.auth.verify,
        };
  const definition: ChannelDefinition<Params, Messages> = {
    type: CHANNEL_SYMBOL,
    paramsSchema: schema,
    auth,
    hasParams: config.paramsSchema !== undefined,
    ...(config.handler !== undefined && { handler: config.handler, transport: "ws" as const }),
    ...lifecycle(config),
  };

  if (definition.hasParams) {
    const callable = (params: Params) => makeHandle(definition, params);
    return attachMeta(callable, definition) as unknown as ChannelExport<Params, Messages>;
  }

  const handle = makeHandle(definition, {} as Params);
  return attachMeta(handle, definition) as unknown as ChannelExport<Params, Messages>;
}

/** Narrow `value` to a `ChannelExport` produced by `defineChannel`. */
export function isChannelExport(value: unknown): value is ChannelExport<unknown, unknown> {
  return isChannelExportMeta(value);
}

export {
  bindChannelName,
  CHANNEL_SYMBOL,
  isChannelDefinition,
  type ChannelAuthConfig,
  type ChannelAuthResult,
  type ChannelAuthScheme,
  type ChannelDefinition,
  type ChannelHandlerContext,
  type ChannelLifecycleConfig,
  type MessageHandler,
  type VerifyInput,
};
