import type { TakoNetworkChannels, TakoNetworkWorkflows } from "./index";
import type { ChannelMessage } from "./types";
import type { EnqueueOptions } from "./workflows/engine";
import { TakoError } from "./tako/error";

/** A portable, type-only workflow contract. Export it from a shared package. */
export interface NetworkWorkflow<Address extends string = string, Payload = unknown> {
  readonly kind: "workflow";
  readonly address: Address;
  readonly payload: Payload;
}

/** A portable, type-only channel contract, including its parameters. */
export interface NetworkChannel<
  Address extends string = string,
  Params = Record<string, never>,
  Messages = Record<string, unknown>,
> {
  readonly kind: "channel";
  readonly address: Address;
  readonly params: Params;
  readonly messages: Messages;
}

export type NetworkRequest =
  | {
      operation: "enqueue";
      payload: unknown;
      opts: { run_at_ms?: number; max_attempts?: number; id?: string };
    }
  | { operation: "publish"; message: { type: string; data: unknown }; params: unknown };

type NetworkCaller = (address: string, request: NetworkRequest) => Promise<unknown>;
let caller: NetworkCaller | null = null;

/** @internal Installed only by the server and worker runtimes. */
export function setNetworkCaller(value: NetworkCaller | null): void {
  caller = value;
}

async function call(address: string, request: NetworkRequest): Promise<unknown> {
  if (!caller) {
    throw new TakoError("TAKO_UNAVAILABLE", "Network primitives require the Tako server runtime.");
  }
  return caller(address, request);
}

type Names<Registry> = keyof Registry extends never ? string : keyof Registry & string;
type Payload<Name extends string> = Name extends keyof TakoNetworkWorkflows
  ? TakoNetworkWorkflows[Name] extends NetworkWorkflow<string, infer P>
    ? P
    : never
  : unknown;
type Params<Name extends string> = Name extends keyof TakoNetworkChannels
  ? TakoNetworkChannels[Name] extends NetworkChannel<string, infer P, unknown>
    ? P
    : never
  : Record<string, unknown>;
type Messages<Name extends string> = Name extends keyof TakoNetworkChannels
  ? TakoNetworkChannels[Name] extends NetworkChannel<string, unknown, infer M>
    ? M
    : never
  : Record<string, unknown>;

/** Address a workflow by local name or `app.workflow`. Resolves in the current environment. */
export function workflow<Name extends Names<TakoNetworkWorkflows>>(address: Name) {
  return {
    async enqueue(payload: Payload<Name>, options: EnqueueOptions = {}): Promise<string> {
      const result = await call(address, {
        operation: "enqueue",
        payload: payload ?? null,
        opts: {
          ...(options.runAt !== undefined && { run_at_ms: options.runAt.getTime() }),
          ...(options.retries !== undefined && { max_attempts: options.retries + 1 }),
          ...(options.id !== undefined && { id: options.id }),
        },
      });
      if (
        !result ||
        typeof result !== "object" ||
        !("id" in result) ||
        typeof result.id !== "string"
      ) {
        throw new TakoError("TAKO_PROTOCOL", "Invalid workflow enqueue response.");
      }
      return result.id;
    },
  };
}

/** Publish to a channel owned by this app or another app on the private network. */
export function channel<Name extends Names<TakoNetworkChannels>>(
  address: Name,
  ...args: Record<string, never> extends Params<Name>
    ? [params?: Params<Name>]
    : [params: Params<Name>]
) {
  return {
    async publish<Type extends keyof Messages<Name> & string>(message: {
      type: Type;
      data: Messages<Name>[Type];
    }): Promise<ChannelMessage<Messages<Name>[Type]>> {
      return (await call(address, {
        operation: "publish",
        message,
        params: args[0] ?? {},
      })) as ChannelMessage<Messages<Name>[Type]>;
    },
  };
}
