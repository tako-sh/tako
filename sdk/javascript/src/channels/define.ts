import { Type, type Static, type TSchema } from "@sinclair/typebox";
import { Channel } from "../channels";
import {
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
  ChannelLiveTransport,
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
  /**
   * TypeBox schema for query params required to bind this channel.
   *
   * Omit for an unparameterized channel.
   */
  paramsSchema?: (t: typeof Type) => ParamsSchema extends TSchema ? ParamsSchema : TSchema;
  /**
   * Authorization policy for subscribe, publish, and connect operations.
   *
   * Omit or set to `false` for a public channel.
   */
  auth?: false | ChannelAuthConfig<Params>;
  /**
   * Optional WebSocket message handlers. Presence of a handler map makes the
   * channel connectable over WebSocket; otherwise browser subscribers use SSE.
   */
  handler?: ChannelHandlerMap<Params, Messages> | undefined;
}

type ChannelHandlerMap<Params, Messages> = {
  [T in keyof Messages]?: MessageHandler<Messages[T], Params>;
};

type ConfigMessageHandler<Data, Params> = {
  handle(data: Data, ctx: ChannelHandlerContext<Params>): Data | void | Promise<Data | void>;
}["handle"];

type ConfigChannelHandlerMap<Params> = Record<string, ConfigMessageHandler<unknown, Params>>;

type ChannelConfigWithParams<ParamsSchema extends TSchema> = Omit<
  ChannelConfig<ParamsSchema, Static<ParamsSchema>, Record<string, unknown>>,
  "handler" | "paramsSchema"
> & {
  paramsSchema: (t: typeof Type) => ParamsSchema;
  handler?: ConfigChannelHandlerMap<Static<ParamsSchema>> | undefined;
};

type ChannelConfigWithoutParams = Omit<
  ChannelConfig<undefined, Record<string, never>, Record<string, unknown>>,
  "handler" | "paramsSchema"
> & {
  paramsSchema?: undefined;
  handler?: ConfigChannelHandlerMap<Record<string, never>> | undefined;
};

type AnyChannelConfig = ChannelLifecycleConfig & {
  name: string;
  paramsSchema?: ((t: typeof Type) => TSchema) | undefined;
  auth?:
    | false
    | {
        headerName?: string | false;
        cookieName?: string;
        verify: unknown;
      };
  handler?: Record<string, unknown> | undefined;
};

/**
 * Bound channel handle returned by {@link defineChannel}.
 *
 * Parameterized channels return this after calling the exported channel with
 * params. Unparameterized channels export the handle directly.
 */
export type ChannelHandle<
  Params,
  Messages,
  Transport extends ChannelLiveTransport = ChannelLiveTransport,
> = {
  readonly __params?: Params;
  readonly __messages?: Messages;
  /** Channel name plus encoded params, useful for logging and diagnostics. */
  readonly name: string;
  /**
   * Publish a typed message to current channel subscribers.
   */
  publish<T extends keyof Messages & string>(
    message: { type: T; data: Messages[T] },
    options?: ChannelPublishOptions,
  ): Promise<ChannelMessage<Messages[T]>>;
  /**
   * Subscribe to messages with the default browser transport.
   */
  subscribe(options?: ChannelSubscribeOptions): ChannelSubscription;
  /**
   * Open a WebSocket connection. Present only when the channel has handlers.
   */
} & (Transport extends "ws"
  ? { connect(options?: ChannelConnectOptions): ChannelSocket }
  : { readonly connect?: never });

/**
 * Metadata attached to every channel export for Tako's discovery pass.
 */
export interface ChannelExportMeta<
  Params,
  Messages,
  Transport extends ChannelLiveTransport = ChannelLiveTransport,
> {
  readonly definition: ChannelDefinition<Params, Messages>;
  /**
   * Narrow the message map for this channel without changing runtime behavior.
   */
  $messageTypes<NewMessages>(): ChannelExport<Params, NewMessages, Transport>;
}

/**
 * Public shape exported from a `<app_root>/channels/<name>.ts` file.
 *
 * Channels with params are callable and return a {@link ChannelHandle}; channels
 * without params are already bound handles.
 */
export type ChannelExport<
  Params,
  Messages,
  Transport extends ChannelLiveTransport = ChannelLiveTransport,
> = (Record<string, never> extends Params
  ? ChannelHandle<Params, Messages, Transport>
  : (params: Params) => ChannelHandle<Params, Messages, Transport>) &
  ChannelExportMeta<Params, Messages, Transport>;

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

function makeHandle<P, M, Transport extends ChannelLiveTransport>(
  definition: ChannelDefinition<P, M>,
  params: P,
): ChannelHandle<P, M, Transport> {
  const query = encodeParams(params as Record<string, unknown>);
  const makeChannel = () =>
    new Channel(definition.channel, definition.transport, params as Record<string, unknown>);
  const handle = {
    get name() {
      return `${definition.channel}${query}`;
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
  return handle as ChannelHandle<P, M, Transport>;
}

function attachMeta<P, M, Transport extends ChannelLiveTransport, T extends object>(
  target: T,
  definition: ChannelDefinition<P, M>,
): T & ChannelExportMeta<P, M, Transport> {
  Object.defineProperty(target, "definition", {
    value: definition,
    writable: false,
    enumerable: false,
    configurable: false,
  });
  Object.defineProperty(target, "$messageTypes", {
    value: function messageTypesNarrow<NewM>() {
      return this as unknown as ChannelExport<P, NewM, Transport>;
    },
    writable: false,
    enumerable: false,
    configurable: false,
  });
  return target as T & ChannelExportMeta<P, M, Transport>;
}

/**
 * Define a typed realtime channel.
 *
 * Put one default export in each `<app_root>/channels/*.ts` file. The optional
 * `paramsSchema` controls the typed params needed to bind the channel, `auth`
 * controls subscribe/publish/connect authorization, and `handler` enables
 * WebSocket messages.
 *
 * @example
 * ```ts
 * import { defineChannel } from "tako.sh";
 *
 * type Messages = { msg: { text: string } };
 *
 * export default defineChannel("chat", {
 *   paramsSchema: (t) => t.Object({ roomId: t.String() }),
 * }).$messageTypes<Messages>();
 * ```
 */
export function defineChannel<ParamsSchema extends TSchema>(
  name: string,
  config: ChannelConfigWithParams<ParamsSchema> & {
    handler: ConfigChannelHandlerMap<Static<ParamsSchema>>;
  },
): ChannelExport<Static<ParamsSchema>, Record<string, unknown>, "ws">;
export function defineChannel<ParamsSchema extends TSchema>(
  name: string,
  config: ChannelConfigWithParams<ParamsSchema> & { handler?: undefined },
): ChannelExport<Static<ParamsSchema>, Record<string, unknown>, "sse">;
export function defineChannel(
  name: string,
  config: ChannelConfigWithoutParams & {
    handler: ConfigChannelHandlerMap<Record<string, never>>;
  },
): ChannelExport<Record<string, never>, Record<string, unknown>, "ws">;
export function defineChannel(
  name: string,
  config?: ChannelConfigWithoutParams & { handler?: undefined },
): ChannelExport<Record<string, never>, Record<string, unknown>, "sse">;
export function defineChannel(name: string, maybeConfig?: unknown): unknown {
  const config = { ...(maybeConfig as object | undefined), name } as AnyChannelConfig;
  const schema = config.paramsSchema?.(Type) ?? Type.Object({});
  const auth: ChannelDefinition<unknown, Record<string, unknown>>["auth"] =
    config.auth === undefined || config.auth === false
      ? false
      : {
          headerName:
            config.auth.headerName === undefined ? "authorization" : config.auth.headerName,
          ...(config.auth.cookieName !== undefined && { cookieName: config.auth.cookieName }),
          verify: config.auth.verify as ChannelAuthConfig<unknown>["verify"],
        };
  const definition: ChannelDefinition<unknown, Record<string, unknown>> = {
    type: CHANNEL_SYMBOL,
    channel: config.name,
    paramsSchema: schema,
    auth,
    hasParams: config.paramsSchema !== undefined,
    ...(config.handler !== undefined
      ? {
          handler: config.handler as NonNullable<
            ChannelDefinition<unknown, Record<string, unknown>>["handler"]
          >,
          transport: "ws" as const,
        }
      : {}),
    ...lifecycle(config),
  };

  if (definition.hasParams) {
    const callable = (params: unknown) => makeHandle(definition, params);
    return attachMeta(callable, definition);
  }

  const handle = makeHandle(definition, {});
  return attachMeta(handle, definition);
}

/** Narrow `value` to a `ChannelExport` produced by `defineChannel`. */
export function isChannelExport(
  value: unknown,
): value is ChannelExport<unknown, unknown, ChannelLiveTransport> {
  return isChannelExportMeta(value);
}

export {
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
