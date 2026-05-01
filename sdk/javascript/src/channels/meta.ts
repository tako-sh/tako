import type {
  ChannelHeaderValue,
  ChannelMessage,
  ChannelOperation,
  ChannelPublishOptions,
} from "../types";

export const CHANNEL_SYMBOL = Symbol.for("tako.channel");

export type ChannelAuthResult = boolean | { subject?: string };

export interface VerifyInput<Params = Record<string, unknown>> {
  channel: string;
  operation: ChannelOperation;
  params: Params;
  header?: ChannelHeaderValue;
  cookie?: string;
}

export interface ChannelAuthConfig<Params> {
  headerName?: string | false;
  cookieName?: string;
  verify: (input: VerifyInput<Params>) => ChannelAuthResult | Promise<ChannelAuthResult>;
}

export interface ChannelHandlerContext<Params = Record<string, unknown>> {
  channel: string;
  operation: ChannelOperation;
  params: Params;
  subject?: string;
  publishedBy: "server" | "client";
}

export type MessageHandler<Data, Params> = (
  data: Data,
  ctx: ChannelHandlerContext<Params>,
) => Data | void | Promise<Data | void>;

export interface ChannelLifecycleConfig {
  replayWindowMs?: number;
  inactivityTtlMs?: number;
  keepaliveIntervalMs?: number;
  maxConnectionLifetimeMs?: number;
}

export type ChannelAuthScheme<Params> =
  | false
  | {
      headerName?: string | false;
      cookieName?: string;
      verify: ChannelAuthConfig<Params>["verify"];
    };

export interface ChannelDefinition<
  Params = Record<string, unknown>,
  Messages = Record<string, unknown>,
> extends ChannelLifecycleConfig {
  readonly type: typeof CHANNEL_SYMBOL;
  readonly channel: string;
  readonly paramsSchema: object;
  readonly auth: ChannelAuthScheme<Params>;
  readonly handler?: { [T in keyof Messages]?: MessageHandler<Messages[T], Params> };
  readonly transport?: "ws";
  readonly hasParams: boolean;
}

export interface ChannelHandle<Params, Messages> {
  readonly __params?: Params;
  readonly name: string;
  publish<T extends keyof Messages & string>(
    message: { type: T; data: Messages[T] },
    options?: ChannelPublishOptions,
  ): Promise<ChannelMessage<Messages[T]>>;
}

export interface ChannelExportMeta<Params, Messages> {
  readonly definition: ChannelDefinition<Params, Messages>;
}

/** Narrow `value` to an object with channel export metadata. */
export function isChannelExport(value: unknown): value is { definition: ChannelDefinition } {
  return (
    value !== null &&
    (typeof value === "function" || typeof value === "object") &&
    "definition" in (value as object) &&
    isChannelDefinition((value as { definition: unknown }).definition)
  );
}

export function isChannelDefinition(value: unknown): value is ChannelDefinition {
  return (
    typeof value === "object" &&
    value !== null &&
    "type" in value &&
    (value as { type: unknown }).type === CHANNEL_SYMBOL
  );
}
