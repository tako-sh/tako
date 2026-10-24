const RECONNECT_BASE_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;

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
