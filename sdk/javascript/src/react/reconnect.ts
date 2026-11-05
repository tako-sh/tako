const RECONNECT_BASE_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;

export function channelIdentity(
  name: string,
  options: {
    transport?: string;
    params?: Record<string, unknown>;
    baseUrl?: string;
    headers?: Record<string, string>;
    authorization?: string;
  },
): string {
  return JSON.stringify(
    [
      name,
      options.transport ?? "sse",
      options.params ?? {},
      options.baseUrl ?? globalThis.location?.origin,
      Object.fromEntries(new Headers(options.headers)),
      options.authorization,
    ],
    (_key, value: unknown) =>
      value && typeof value === "object" && !Array.isArray(value)
        ? Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)))
        : value,
  );
}

export function terminalClose(code: number): boolean {
  return code === 4401 || code === 4403 || code === 4404;
}

export function reconnectDelay(attempt: number): number {
  const base = Math.min(RECONNECT_BASE_MS * 2 ** attempt, RECONNECT_MAX_MS);
  return base + Math.random() * base * 0.3;
}

export function scheduleReconnect(connect: () => void, delayMs: number): () => void {
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
