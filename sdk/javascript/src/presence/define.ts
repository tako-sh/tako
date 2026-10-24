import type { Static, TSchema } from "@sinclair/typebox";
import { defineChannel, type ChannelConfig } from "../channels/define";
import type { ChannelAuthConfig, ChannelDefinition, VerifyInput } from "../channels/meta";
import { PRESENCE_CHANNEL_PREFIX } from "../presence";

export interface PresenceDefinition<Params = Record<string, never>> {
  readonly name: string;
  readonly definition: ChannelDefinition<Params>;
}

/** Define a presence space in `<app_root>/presence/`. Auth matches channels. */
export function definePresence<S extends TSchema>(
  name: string,
  config: Omit<ChannelConfig<S, Static<S>>, "transport"> &
    Required<Pick<ChannelConfig<S, Static<S>>, "paramsSchema">>,
): PresenceDefinition<Static<S>>;
export function definePresence(
  name: string,
  config?: Omit<ChannelConfig<undefined, Record<string, never>>, "paramsSchema" | "transport">,
): PresenceDefinition;
export function definePresence(name: string, config: object = {}): unknown {
  const auth = (config as { auth?: false | ChannelAuthConfig<unknown> }).auth;
  const channel = defineChannel(`${PRESENCE_CHANNEL_PREFIX}${name}`, {
    ...config,
    ...(auth && {
      auth: {
        ...auth,
        verify: (input: VerifyInput<unknown>) => auth.verify({ ...input, channel: name }),
      },
    }),
    transport: "ws",
  });
  return { name, definition: channel.definition };
}
