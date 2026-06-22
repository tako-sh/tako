/**
 * React hooks for Tako channels.
 *
 * Browser-safe entry — imports only `./channels` and `./types` beneath React,
 * so Vite can bundle it for the client without pulling in server-only
 * modules.
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
 * Or connect via WebSocket:
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
 * The `messages` buffer is capped at the last 500 entries. `publish` is a
 * one-shot fetch — use `channel.publish(...)` from `tako.sh/client` directly
 * rather than a hook.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Channel } from "./channels";
import type { ChannelConnectOptions, ChannelMessage, ChannelSubscribeOptions } from "./types";

const MAX_MESSAGES = 500;
const RECONNECT_BASE_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;

type Status = "connecting" | "open";
type MessageHandler<T> = (msg: ChannelMessage<T>) => void;

function reconnectDelay(attempt: number): number {
  const base = Math.min(RECONNECT_BASE_MS * 2 ** attempt, RECONNECT_MAX_MS);
  return base + Math.random() * base * 0.3;
}

function scheduleReconnect(connect: () => void, delayMs: number): () => void {
  let cancelled = false;
  const run = () => {
    if (cancelled) return;
    cancelled = true;
    clearTimeout(timer);
    globalThis.removeEventListener?.("online", run);
    connect();
  };
  const timer = setTimeout(run, delayMs);
  globalThis.addEventListener?.("online", run, { once: true });
  return () => {
    cancelled = true;
    clearTimeout(timer);
    globalThis.removeEventListener?.("online", run);
  };
}

function appendCapped<T>(buffer: ChannelMessage<T>[], msg: ChannelMessage<T>): ChannelMessage<T>[] {
  if (buffer.length < MAX_MESSAGES) return [...buffer, msg];
  return [...buffer.slice(buffer.length - MAX_MESSAGES + 1), msg];
}

/** Live React state for one channel subscription or WebSocket connection. */
export interface ChannelConnection<T = unknown> {
  /** All messages received since mount or the last `clear()`, capped at 500. */
  readonly messages: ChannelMessage<T>[];
  /** `"connecting"` before the first open or while reconnecting; `"open"` otherwise. */
  readonly status: Status;
  /** Last error seen (parse failure or transport error). Cleared on successful reconnect. */
  readonly error: Error | null;
  /** Empty the message buffer. */
  clear(): void;
  /** Send a payload over the underlying WebSocket. Present only when `transport: "ws"`. */
  send?(data: unknown): void;
}

interface BaseHookOptions<T> {
  /** Typed channel params serialized into the channel query string. */
  params?: Record<string, unknown>;
  /**
   * Called for each incoming message. The latest handler is always used —
   * you do not need to memoize it.
   */
  onMessage?: MessageHandler<T>;
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

  const [messages, setMessages] = useState<ChannelMessage<T>[]>([]);
  const [status, setStatus] = useState<Status>("connecting");
  const [error, setError] = useState<Error | null>(null);

  const optionsRef = useRef(options);
  optionsRef.current = options;
  const paramsKey = JSON.stringify(options.params ?? {});

  const handlerRef = useRef<MessageHandler<T> | undefined>(options.onMessage);
  handlerRef.current = options.onMessage;

  const socketRef = useRef<{ send(data: unknown): void } | null>(null);
  const lastMessageIdRef = useRef<string | undefined>(
    (options as ChannelConnectOptions).lastMessageId,
  );

  const handleIncoming = useCallback((raw: string) => {
    try {
      const parsed = JSON.parse(raw) as ChannelMessage<T>;
      lastMessageIdRef.current = parsed.id;
      setMessages((prev) => appendCapped(prev, parsed));
      const handler = handlerRef.current;
      if (handler) {
        try {
          handler(parsed);
        } catch (err) {
          console.error("useChannel onMessage handler threw:", err);
        }
      }
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    }
  }, []);

  useEffect(() => {
    if (transport === "ws") {
      let disposed = false;
      let currentClose: (() => void) | null = null;
      let cancelReconnect: (() => void) | null = null;
      let attempt = 0;

      const connect = () => {
        if (disposed) return;

        const channel = new Channel(name, "ws", optionsRef.current.params ?? {});
        const currentOptions = optionsRef.current as ChannelConnectOptions;
        const resumeFrom = lastMessageIdRef.current ?? currentOptions.lastMessageId;
        const conn = channel.connect({
          ...currentOptions,
          ...(resumeFrom !== undefined && { lastMessageId: resumeFrom }),
        });
        const target = conn.raw as EventTarget;
        socketRef.current = conn;
        currentClose = () => conn.close();

        const handleOpen = () => {
          attempt = 0;
          setStatus("open");
          setError(null);
        };
        const handleMessage = (e: Event) => {
          handleIncoming((e as MessageEvent).data);
        };
        const handleError = () => {
          setError(new Error(`channel "${name}" connection error`));
        };
        const handleClose = () => {
          target.removeEventListener("open", handleOpen);
          target.removeEventListener("message", handleMessage);
          target.removeEventListener("error", handleError);
          target.removeEventListener("close", handleClose);
          socketRef.current = null;
          currentClose = null;
          if (disposed) return;
          setStatus("connecting");
          const delay = reconnectDelay(attempt++);
          cancelReconnect = scheduleReconnect(connect, delay);
        };

        target.addEventListener("open", handleOpen);
        target.addEventListener("message", handleMessage);
        target.addEventListener("error", handleError);
        target.addEventListener("close", handleClose);
      };

      connect();

      return () => {
        disposed = true;
        cancelReconnect?.();
        currentClose?.();
        socketRef.current = null;
      };
    }

    // SSE path — SseReader keeps the fetch-based stream connected.
    const channel = new Channel(name, undefined, optionsRef.current.params ?? {});
    const sub = channel.subscribe(optionsRef.current as ChannelSubscribeOptions);
    const target = sub.raw as EventTarget;

    const handleOpen = () => {
      setStatus("open");
      setError(null);
    };
    const handleError = () => {
      setStatus("connecting");
      setError(new Error(`channel "${name}" subscription error`));
    };
    const handleMessage = (e: Event) => {
      handleIncoming((e as MessageEvent).data);
    };

    target.addEventListener("open", handleOpen);
    target.addEventListener("message", handleMessage);
    target.addEventListener("error", handleError);

    return () => {
      target.removeEventListener("open", handleOpen);
      target.removeEventListener("message", handleMessage);
      target.removeEventListener("error", handleError);
      sub.close();
    };
  }, [name, transport, paramsKey, handleIncoming]);

  const clear = useCallback(() => setMessages([]), []);
  const send = useCallback((data: unknown) => {
    socketRef.current?.send(data);
  }, []);

  const result: ChannelConnection<T> = { messages, status, error, clear };
  if (transport === "ws") {
    result.send = send;
  }
  return result;
}
