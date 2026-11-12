import { Type, type Static, type TSchema } from "@sinclair/typebox";
import { Channel, definitionLifecycleConfig } from "../channels";
import { assertFlatChannelParamsSchema, channelParamQuery } from "./params";
import {
  CHANNEL_SYMBOL,
  isChannelDefinition,
  isChannelExport as isChannelExportMeta,
  type ChannelAuthConfig,
  type ChannelAuthResult,
  type ChannelAuthScheme,
  type ChannelDefinition,
  type ChannelLifecycleConfig,
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

/** Options passed to {@link defineChannel}. */
export interface ChannelConfig<
  ParamsSchema extends TSchema | undefined,
  Params,
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
   * @defaultValue false
   */
  auth?: false | ChannelAuthConfig<Params>;
  /** Enable client publishing over WebSocket. Omit for SSE subscriptions. */
  transport?: "ws";
}

type ChannelConfigWithParams<ParamsSchema extends TSchema> = Omit<
  ChannelConfig<ParamsSchema, Static<ParamsSchema>>,
  "paramsSchema"
> & {
  paramsSchema: (t: typeof Type) => ParamsSchema;
};

type ChannelConfigWithoutParams = Omit<
  ChannelConfig<undefined, Record<string, never>>,
  "paramsSchema"
> & {
  paramsSchema?: undefined;
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
  transport?: "ws";
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
  /** Type-only params marker used by generated declarations. */
  readonly __params?: Params;
  /** Type-only message-map marker used by generated declarations. */
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
   * Open a WebSocket connection. Present only with transport: "ws".
   */
} & (Transport extends "ws"
  ? {
      /** Open a WebSocket connection. */
      connect(options?: ChannelConnectOptions): ChannelSocket;
    }
  : { readonly connect?: never });

/**
 * Metadata attached to every channel export for Tako's discovery pass.
 */
export interface ChannelExportMeta<
  Params,
  Transport extends ChannelLiveTransport = ChannelLiveTransport,
> {
  readonly definition: ChannelDefinition<Params>;
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
  ChannelExportMeta<Params, Transport>;

function lifecycle(config: ChannelLifecycleConfig): ChannelLifecycleConfig {
  if (config.ephemeral && config.replayWindowMs !== undefined) {
    throw new Error("ephemeral channels cannot set replayWindowMs");
  }
  return {
    ...(config.ephemeral !== undefined && { ephemeral: config.ephemeral }),
    ...(config.replayWindowMs !== undefined && {
      replayWindowMs: config.replayWindowMs,
    }),
    ...(config.inactivityTtlMs !== undefined && {
      inactivityTtlMs: config.inactivityTtlMs,
    }),
    ...(config.keepaliveIntervalMs !== undefined && {
      keepaliveIntervalMs: config.keepaliveIntervalMs,
    }),
    ...(config.maxConnectionLifetimeMs !== undefined && {
      maxConnectionLifetimeMs: config.maxConnectionLifetimeMs,
    }),
  };
}

function encodeParams(params: Record<string, unknown>): string {
  const query = new URLSearchParams(channelParamQuery(params)).toString();
  return query ? `?${query}` : "";
}

function makeHandle<P, M, Transport extends ChannelLiveTransport>(
  definition: ChannelDefinition<P>,
  params: P,
): ChannelHandle<P, M, Transport> {
  const query = encodeParams(params as Record<string, unknown>);
  const makeChannel = () =>
    new Channel(
      definition.channel,
      definition.transport,
      params as Record<string, unknown>,
      definitionLifecycleConfig(definition),
      definition.paramsSchema,
    );
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

function attachMeta<P, Transport extends ChannelLiveTransport, T extends object>(
  target: T,
  definition: ChannelDefinition<P>,
): T & ChannelExportMeta<P, Transport> {
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
  return target as T & ChannelExportMeta<P, Transport>;
}

/**
 * Define a typed realtime channel.
 *
 * Put one default export in each `<app_root>/channels/*.ts` file. The optional
 * `paramsSchema` controls the typed params needed to bind the channel, `auth`
 * controls subscribe/publish/connect authorization, and `transport: "ws"`
 * enables client publishing over WebSocket.
 *
 * Params select the stream: `chat({ roomId: "a" })` and `chat({ roomId: "b" })`
 * have separate messages, replay, and subscribers. Params are flat string,
 * number, or boolean values. `auth.verify` must still check that the caller
 * may use the requested params.
 *
 * @example
 * ```ts
 * import { defineChannel } from "tako.sh";
 *
 * type Messages = { msg: { text: string } };
 *
 * export default defineChannel("chat", {
 *   paramsSchema: (t) => t.Object({ roomId: t.String() }),
 *   auth: {
 *     verify: ({ header, params }) => canJoinRoom(header, params.roomId),
 *   },
 * }).$messageTypes<Messages>();
 *
 * await chat({ roomId: "a" }).publish({ type: "msg", data: { text: "hi" } });
 * ```
 */
export function defineChannel<ParamsSchema extends TSchema>(
  name: string,
  config: ChannelConfigWithParams<ParamsSchema> & {
    transport: "ws";
  },
): ChannelExport<Static<ParamsSchema>, Record<string, unknown>, "ws">;
/** Define a parameterized SSE channel. */
export function defineChannel<ParamsSchema extends TSchema>(
  name: string,
  config: ChannelConfigWithParams<ParamsSchema> & { transport?: undefined },
): ChannelExport<Static<ParamsSchema>, Record<string, unknown>, "sse">;
/** Define an unparameterized WebSocket channel. */
export function defineChannel(
  name: string,
  config: ChannelConfigWithoutParams & {
    transport: "ws";
  },
): ChannelExport<Record<string, never>, Record<string, unknown>, "ws">;
/** Define an unparameterized SSE channel. */
export function defineChannel(
  name: string,
  config?: ChannelConfigWithoutParams & { transport?: undefined },
): ChannelExport<Record<string, never>, Record<string, unknown>, "sse">;
/** Implementation signature for {@link defineChannel}. */
export function defineChannel(name: string, maybeConfig?: unknown): unknown {
  const config = {
    ...(maybeConfig as object | undefined),
    name,
  } as AnyChannelConfig;
  const schema = config.paramsSchema?.(Type) ?? Type.Object({});
  // Store reads carry params as JSON; channel and presence params are query values.
  if (!config.name.startsWith("tako.store:")) {
    assertFlatChannelParamsSchema(config.name, schema);
  }
  const auth: ChannelDefinition<unknown>["auth"] =
    config.auth === undefined || config.auth === false
      ? false
      : {
          headerName:
            config.auth.headerName === undefined ? "authorization" : config.auth.headerName,
          ...(config.auth.cookieName !== undefined && {
            cookieName: config.auth.cookieName,
          }),
          verify: config.auth.verify as ChannelAuthConfig<unknown>["verify"],
        };
  const definition: ChannelDefinition<unknown> = {
    type: CHANNEL_SYMBOL,
    channel: config.name,
    paramsSchema: schema,
    auth,
    hasParams: config.paramsSchema !== undefined,
    ...(config.transport !== undefined && { transport: config.transport }),
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
  type ChannelLifecycleConfig,
  type VerifyInput,
};
