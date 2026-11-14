/**
 * React hooks for Tako channels.
 *
 * Browser-safe entry — imports only `./channels` and `./types` beneath React,
 * so Vite can bundle it for the client without pulling in server-only
 * modules.
 *
 * `params` select the channel stream: `{ roomId: "a" }` and `{ roomId: "b" }`
 * receive separate messages, and the server publishes to a room with
 * `chat({ roomId }).publish(...)`. The channel's `auth.verify` must still
 * check that the caller may use the requested params.
 *
 * Subscribe via SSE (default):
 * @example
 * ```typescript
 * import { useChannel } from "tako.sh/react";
 *
 * function ChatRoom({ room }: { room: string }) {
 *   const { messages, status, error } = useChannel("chat", {
 *     params: { roomId: room },
 *   });
 *   // ...
 * }
 * ```
 *
 * Or enable client publishing over the shared connection:
 * @example
 * ```typescript
 * const { messages, status, send } = useChannel("chat", {
 *   params: { roomId: room },
 *   transport: "ws",
 * });
 * ```
 *
 * React imperatively to each incoming message via the `onMessage` option.
 * The handler always sees the latest closure; no dependency wiring needed:
 * @example
 * ```typescript
 * useChannel("notifications", {
 *   onMessage: (msg) => toast(msg.data.text),
 * });
 * ```
 *
 * Read a stream from its start and stop when the server closes it, such as
 * a streamed reply that must survive a page refresh:
 * @example
 * ```typescript
 * const { messages, closed } = useChannel<{ content: string }>("reply", {
 *   params: { replyId },
 *   from: "start",
 * });
 * // Each message carries the whole text so far.
 * const text = messages.at(-1)?.data.content ?? "";
 * ```
 *
 * The `messages` buffer is capped at the last 500 entries. `publish` is a
 * one-shot fetch — use `channel.publish(...)` from `tako.sh/client` directly
 * rather than a hook.
 */

import { useCallback, useEffect, useInsertionEffect, useMemo, useRef, useState } from "react";
import {
  CHANNEL_STREAM_CLOSED_CODE,
  Channel,
  type ChannelSubscribeOptions,
  type SseRecoveryReason,
} from "./channels";
import type { ChannelConnectOptions, ChannelMessage } from "./types";

import {
  channelIdentity,
  reconnectDelay,
  scheduleReconnect,
  terminalClose,
} from "./react/reconnect";
export { usePresence, type UsePresenceOptions, type PresenceConnection } from "./react/presence";
export type { PresenceMember } from "./presence";

const MAX_MESSAGES = 500;
type Status = "connecting" | "open" | "closed";
type MessageHandler<T> = (msg: ChannelMessage<T>) => void;

function appendCapped<T>(buffer: ChannelMessage<T>[], msg: ChannelMessage<T>): ChannelMessage<T>[] {
  if (buffer.length < MAX_MESSAGES) return [...buffer, msg];
  return [...buffer.slice(buffer.length - MAX_MESSAGES + 1), msg];
}

/** Live React state for one channel subscription or WebSocket connection. */
export interface ChannelConnection<T = unknown> {
  /** All messages received since mount or the last `clear()`, capped at 500. */
  readonly messages: ChannelMessage<T>[];
  /**
   * `"connecting"` before the first open or while reconnecting; `"open"` while
   * live; `"closed"` once the server closed the stream (final).
   */
  readonly status: Status;
  /**
   * `true` once the server closed the stream, after every earlier message was
   * delivered. The hook does not reconnect; `send()` then throws.
   */
  readonly closed: boolean;
  /** Last error seen (parse failure or transport error). Cleared on successful reconnect. */
  readonly error: Error | null;
  /** Empty the message buffer. */
  clear(): void;
  /**
   * Send a payload over the underlying WebSocket. Present only when
   * `transport: "ws"`. Throws once the stream is closed.
   */
  send?(data: unknown): void;
}

interface BaseHookOptions<T> {
  /**
   * Flat channel params serialized into the query string. They select the
   * stream; changing them reconnects to the other stream.
   */
  params?: Record<string, unknown>;
  /**
   * Called for each incoming message. The latest handler is always used —
   * you do not need to memoize it.
   */
  onMessage?: MessageHandler<T>;
  /**
   * Reports automatic recovery outside the message stream. With
   * `from: "start"`, `"history-expired"` means the stream replays from its
   * start: discard state built from earlier messages (for example call
   * `clear()`), since message ids can change across the reset.
   */
  onRecovery?: (reason: SseRecoveryReason) => void;
  /**
   * Reload application state when the replay history has expired. `messages`
   * keeps what arrived before; with `from: "start"` call `clear()` here, or
   * the replayed messages follow the old ones.
   */
  onResync?: () => void;
  /**
   * Called once when the server closed the stream. The latest handler is
   * always used. `from: "start"` (transport option) replays retained messages
   * first; reconnects within one mount resume by cursor.
   */
  onClose?: () => void;
}

/**
 * Options for {@link useChannel}.
 *
 * Discriminated on `transport`. The default is `"sse"`, which accepts the
 * {@link ChannelSubscribeOptions} surface; `"ws"` accepts
 * {@link ChannelConnectOptions} instead and exposes `send()` on the result.
 */
export type UseChannelOptions<T = unknown> =
  | (BaseHookOptions<T> & { transport?: "sse" } & ChannelSubscribeOptions)
  | (BaseHookOptions<T> & { transport: "ws" } & ChannelConnectOptions);

/**
 * Subscribe a React component to a Tako channel.
 *
 * Returns a live {@link ChannelConnection} whose `messages` buffer updates as
 * frames arrive (capped at the last 500). The hook reconnects automatically
 * with exponential backoff + jitter on transport errors, and the latest
 * `onMessage` handler is always invoked — no memoization needed.
 *
 * With `transport: "ws"` the returned object additionally has a `send(data)`
 * method for pushing to the server.
 *
 * @typeParam T - The message payload type.
 * @param name - The exact channel name to subscribe to (e.g. `"chat:room-1"`).
 * @param options - Transport selection, an `onMessage` handler, and transport-specific options.
 * @returns A {@link ChannelConnection} reflecting current state.
 */
/** Subscribe to a channel over SSE. */
export function useChannel<T = unknown>(
  name: string,
  options?: BaseHookOptions<T> & { transport?: "sse" } & ChannelSubscribeOptions,
): ChannelConnection<T>;
/** Connect to a channel over WebSocket. */
export function useChannel<T = unknown>(
  name: string,
  options: BaseHookOptions<T> & { transport: "ws" } & ChannelConnectOptions,
): ChannelConnection<T> & { send(data: unknown): void };
/** Implementation signature for {@link useChannel}. */
export function useChannel<T = unknown>(
  name: string,
  options: UseChannelOptions<T> = {},
): ChannelConnection<T> {
  const transport = options.transport ?? "sse";

  const key = channelIdentity(name, options);
  const identity = useMemo(() => ({ key }), [key]);
  const latestIdentity = useRef(identity);
  const optionsRef = useRef(options);
  useInsertionEffect(() => {
    latestIdentity.current = identity;
    optionsRef.current = options;
  });
  const [state, setState] = useState<{
    identity: typeof identity;
    messages: ChannelMessage<T>[];
    status: Status;
    error: Error | null;
  }>({ identity, messages: [], status: "connecting", error: null });
  const socketRef = useRef<{
    identity: typeof identity;
    send(data: unknown): void;
  } | null>(null);
  const closedRef = useRef<typeof identity | null>(null);

  useEffect(() => {
    let disposed = false;
    const currentOptions = optionsRef.current;
    const active = () => !disposed && latestIdentity.current === identity;
    let lastMessageId = (currentOptions as ChannelConnectOptions).lastMessageId;
    const seen = new Set<string>();
    let closed = false;
    setState({ identity, messages: [], status: "connecting", error: null });

    const update = (status: Status, error: Error | null = null) => {
      if (active() && !closed) setState((previous) => ({ ...previous, status, error }));
    };
    // The server closed the stream after its last message: final, no reconnect.
    const closeStream = () => {
      if (!active() || closed) return;
      update("closed");
      closed = true;
      closedRef.current = identity;
      try {
        optionsRef.current.onClose?.();
      } catch (error) {
        console.error("useChannel onClose handler threw:", error);
      }
    };
    const recover = (reason: SseRecoveryReason) => {
      if (!active() || closed) return;
      update("connecting");
      try {
        void Promise.resolve(optionsRef.current.onRecovery?.(reason)).catch((error) => {
          console.error("useChannel onRecovery handler threw:", error);
        });
      } catch (error) {
        console.error("useChannel onRecovery handler threw:", error);
      }
      if (active() && reason === "history-expired") {
        try {
          void Promise.resolve(optionsRef.current.onResync?.()).catch((error) => {
            console.error("useChannel onResync handler threw:", error);
          });
        } catch (error) {
          console.error("useChannel onResync handler threw:", error);
        }
      }
    };
    const incoming = (raw: string) => {
      if (!active()) return;
      try {
        const parsed = JSON.parse(raw) as ChannelMessage<T>;
        const identity = parsed.eventId
          ? `${parsed.eventId.origin}:${parsed.eventId.sequence}`
          : parsed.id;
        if (identity) {
          if (seen.has(identity)) return;
          seen.add(identity);
          if (seen.size > MAX_MESSAGES) seen.delete(seen.values().next().value!);
          if (parsed.id) lastMessageId = parsed.id;
        }
        setState((previous) => ({
          ...previous,
          messages: appendCapped(previous.messages, parsed),
        }));
        try {
          optionsRef.current.onMessage?.(parsed);
        } catch (error) {
          console.error("useChannel onMessage handler threw:", error);
        }
      } catch (error) {
        if (active())
          setState((previous) => ({
            ...previous,
            error: error instanceof Error ? error : new Error(String(error)),
          }));
      }
    };

    if (transport === "ws") {
      let close: (() => void) | undefined;
      let cancelReconnect: (() => void) | undefined;
      let attempt = 0;
      const connect = () => {
        if (!active()) return;
        const channel = new Channel(name, "ws", currentOptions.params ?? {});
        const { lastMessageId: _initial, ...connectionOptions } =
          currentOptions as ChannelConnectOptions;
        const connection = channel.connect({
          ...connectionOptions,
          ...(lastMessageId !== undefined && { lastMessageId }),
          onClose: closeStream,
        });
        const target = connection.raw as EventTarget;
        let connected = true;
        socketRef.current = { identity, send: connection.send };
        const onOpen = () => {
          if (!connected || !active()) return;
          attempt = 0;
          update("open");
        };
        const onMessage = (event: Event) => {
          if (connected) incoming((event as MessageEvent<string>).data);
        };
        const onError = () => {
          if (connected) update("connecting");
        };
        const cleanup = () => {
          connected = false;
          target.removeEventListener("open", onOpen);
          target.removeEventListener("message", onMessage);
          target.removeEventListener("error", onError);
          target.removeEventListener("close", onClose);
          if (socketRef.current?.identity === identity) socketRef.current = null;
        };
        const onClose = (event: Event) => {
          cleanup();
          if (!active()) return;
          const code = (event as CloseEvent).code;
          if (code === CHANNEL_STREAM_CLOSED_CODE) {
            closeStream();
            return;
          }
          if (terminalClose(code)) {
            update("connecting", new Error(`channel "${name}" rejected (${code}).`));
            return;
          }
          if (code === 4410) lastMessageId = undefined;
          recover(code === 4410 ? "history-expired" : "reconnecting");
          if (active()) cancelReconnect = scheduleReconnect(connect, reconnectDelay(attempt++));
        };
        target.addEventListener("open", onOpen);
        target.addEventListener("message", onMessage);
        target.addEventListener("error", onError);
        target.addEventListener("close", onClose);
        close = () => {
          cleanup();
          connection.close();
        };
      };
      connect();
      return () => {
        disposed = true;
        cancelReconnect?.();
        close?.();
      };
    }

    const channel = new Channel(name, undefined, currentOptions.params ?? {});
    const subscription = channel.subscribe({
      ...(currentOptions as ChannelSubscribeOptions),
      onRecovery: recover,
      onClose: closeStream,
      onError: (error) => {
        if (!active()) return;
        update("connecting", error);
        (optionsRef.current as ChannelSubscribeOptions).onError?.(error);
      },
    });
    const target = subscription.raw as EventTarget;
    const onOpen = () => update("open");
    const onError = (event: Event) =>
      update(
        "connecting",
        (event as ErrorEvent).error instanceof Error
          ? (event as ErrorEvent).error
          : new Error(`channel "${name}" subscription error`),
      );
    const onMessage = (event: Event) => incoming((event as MessageEvent<string>).data);
    target.addEventListener("open", onOpen);
    target.addEventListener("message", onMessage);
    target.addEventListener("error", onError);
    return () => {
      disposed = true;
      target.removeEventListener("open", onOpen);
      target.removeEventListener("message", onMessage);
      target.removeEventListener("error", onError);
      subscription.close();
    };
  }, [identity, name, transport]);

  const clear = useCallback(() => {
    if (latestIdentity.current === identity)
      setState((previous) => ({ ...previous, messages: [] }));
  }, [identity]);
  const send = useCallback(
    (data: unknown) => {
      if (closedRef.current === identity) {
        throw new Error(`Channel '${name}' stream is closed; it accepts no more messages.`);
      }
      if (latestIdentity.current === identity && socketRef.current?.identity === identity) {
        socketRef.current.send(data);
      }
    },
    [identity, name],
  );
  const visible =
    state.identity === identity
      ? state
      : { messages: [], status: "connecting" as const, error: null };
  return {
    messages: visible.messages,
    status: visible.status,
    closed: visible.status === "closed",
    error: visible.error,
    clear,
    ...(transport === "ws" && { send }),
  };
}
export { useStore } from "./react/store";
export type { UseStoreOptions, StoreConnection } from "./react/store";
