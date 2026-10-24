import { useCallback, useEffect, useRef, useState } from "react";
import { Channel } from "../channels";
import { PRESENCE_CHANNEL_PREFIX, type PresenceMember, type PresenceSnapshot } from "../presence";
import type { ChannelConnectOptions } from "../types";
import { reconnectDelay, scheduleReconnect } from "./reconnect";

function encodePresenceData(data: unknown): string {
  const encoded = JSON.stringify(data);
  if (encoded === undefined) throw new Error("Presence data must be JSON-serializable.");
  if (new TextEncoder().encode(encoded).length > 16_384) {
    throw new Error("Presence data must be at most 16 KiB.");
  }
  return `{"type":"tako.presence","data":${encoded}}`;
}

export interface UsePresenceOptions<T> extends Omit<ChannelConnectOptions, "lastMessageId"> {
  params?: Record<string, unknown>;
  /** Initial metadata. Call update() to replace it after joining. */
  data: T;
}

export interface PresenceConnection<T> {
  members: PresenceMember<T>[];
  connectionId: string | null;
  status: "connecting" | "open";
  error: Error | null;
  /** Replace this connection's metadata, including on the next reconnect. */
  update(data: T): void;
}

/** Join a presence space. The server derives identity from its auth policy. */
export function usePresence<T>(
  name: string,
  options: UsePresenceOptions<T>,
): PresenceConnection<T> {
  const paramsKey = JSON.stringify(options.params ?? {});
  const headersKey = JSON.stringify(options.headers ?? {});
  const scopeKey = JSON.stringify([name, paramsKey]);
  const connectionKey = JSON.stringify([
    scopeKey,
    headersKey,
    options.baseUrl,
    options.authorization,
  ]);
  const latestKeyRef = useRef(connectionKey);
  latestKeyRef.current = connectionKey;
  const activeKeyRef = useRef<string | null>(null);
  const [stateKey, setStateKey] = useState(connectionKey);
  const [members, setMembers] = useState<PresenceMember<T>[]>([]);
  const [connectionId, setConnectionId] = useState<string | null>(null);
  const [status, setStatus] = useState<"connecting" | "open">("connecting");
  const [error, setError] = useState<Error | null>(null);
  const optionsRef = useRef(options);
  optionsRef.current = options;
  const dataRef = useRef<{ scopeKey: string; data: T; payload?: string }>({
    scopeKey,
    data: options.data,
  });
  const sendRef = useRef<(() => void) | null>(null);

  useEffect(() => {
    if (dataRef.current.scopeKey !== scopeKey) {
      dataRef.current = { scopeKey, data: optionsRef.current.data };
    }
    activeKeyRef.current = connectionKey;
    setStateKey(connectionKey);
    let disposed = false;
    let close: (() => void) | undefined;
    let cancelReconnect: (() => void) | undefined;
    let heartbeat: ReturnType<typeof setInterval> | undefined;
    let attempt = 0;
    setMembers([]);
    setConnectionId(null);
    setStatus("connecting");
    setError(null);

    const connect = () => {
      if (disposed) return;
      const current = optionsRef.current;
      try {
        const connection = new Channel(
          `${PRESENCE_CHANNEL_PREFIX}${name}`,
          "ws",
          current.params,
        ).connect(current);
        const target = connection.raw as EventTarget;
        let ready = false;
        const send = () => {
          if (
            !ready ||
            latestKeyRef.current !== connectionKey ||
            dataRef.current.scopeKey !== scopeKey
          )
            return;
          try {
            dataRef.current.payload ??= encodePresenceData(dataRef.current.data);
            connection.send(dataRef.current.payload);
            heartbeat ??= setInterval(send, 15_000);
          } catch (err) {
            setError(err instanceof Error ? err : new Error(String(err)));
          }
        };
        sendRef.current = send;
        const onMessage = (event: Event) => {
          if (disposed) return;
          try {
            const message = JSON.parse((event as MessageEvent<string>).data) as
              | PresenceSnapshot<T>
              | { type: "tako.presence.ready" };
            if (message.type === "tako.presence.ready") {
              if (ready) return;
              ready = true;
              send();
            } else if (message.type === "tako.presence" && Array.isArray(message.members)) {
              setMembers(message.members);
              setConnectionId(message.connectionId);
              setStatus("open");
              setError(null);
              attempt = 0;
            }
          } catch (err) {
            setError(err instanceof Error ? err : new Error(String(err)));
          }
        };
        const onError = () => setError(new Error(`Presence "${name}" connection failed.`));
        const cleanup = () => {
          ready = false;
          clearInterval(heartbeat);
          heartbeat = undefined;
          sendRef.current = null;
          target.removeEventListener("message", onMessage);
          target.removeEventListener("error", onError);
          target.removeEventListener("close", onClose);
        };
        const onClose = (event: Event) => {
          cleanup();
          if (disposed) return;
          setMembers([]);
          setConnectionId(null);
          setStatus("connecting");
          const code = (event as CloseEvent).code;
          if (code >= 4400 && code < 4500) {
            setError(new Error(`Presence "${name}" rejected (${code}).`));
            return;
          }
          cancelReconnect = scheduleReconnect(connect, reconnectDelay(attempt++));
        };
        target.addEventListener("message", onMessage);
        target.addEventListener("error", onError);
        target.addEventListener("close", onClose);
        close = () => {
          cleanup();
          connection.close();
        };
      } catch (err) {
        setError(err instanceof Error ? err : new Error(String(err)));
        cancelReconnect = scheduleReconnect(connect, reconnectDelay(attempt++));
      }
    };
    connect();
    return () => {
      disposed = true;
      if (activeKeyRef.current === connectionKey) activeKeyRef.current = null;
      cancelReconnect?.();
      clearInterval(heartbeat);
      close?.();
    };
  }, [name, scopeKey, connectionKey]);

  const update = useCallback(
    (data: T) => {
      if (latestKeyRef.current !== connectionKey) return;
      try {
        const payload = encodePresenceData(data);
        dataRef.current = { scopeKey, data, payload };
        if (activeKeyRef.current === connectionKey) sendRef.current?.();
      } catch (err) {
        setError(err instanceof Error ? err : new Error(String(err)));
      }
    },
    [connectionKey, scopeKey],
  );
  return stateKey === connectionKey
    ? { members, connectionId, status, error, update }
    : { members: [], connectionId: null, status: "connecting", error: null, update };
}
