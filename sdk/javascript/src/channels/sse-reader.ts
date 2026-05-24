import { fetchEventSource, type EventSourceMessage } from "@microsoft/fetch-event-source";

export interface SseReaderMessage {
  id?: string;
  type?: string;
  data: string;
}

export interface SseReaderOptions {
  fetch?: typeof fetch;
  headers?: Record<string, string>;
  onMessage: (message: SseReaderMessage) => void;
  onOpen?: () => void;
  onError?: (error: Error) => void;
  signal?: AbortSignal;
  backoffBaseMs?: number;
  backoffMaxMs?: number;
  jitter?: number;
  retryOnDisconnect?: boolean;
}

interface DrainOptions {
  connections?: number;
}

type Listener = EventListenerOrEventListenerObject;
type FetchEventSourceGlobal = typeof globalThis & {
  window?: typeof globalThis;
  document?: {
    hidden: boolean;
    addEventListener(type: string, listener: EventListenerOrEventListenerObject): void;
    removeEventListener(type: string, listener: EventListenerOrEventListenerObject): void;
  };
};

interface FetchEventSourceGlobalSnapshot {
  global: FetchEventSourceGlobal;
  hadWindow: boolean;
  previousWindow: typeof globalThis | undefined;
  hadDocument: boolean;
  previousDocument: FetchEventSourceGlobal["document"];
}

let fetchEventSourceGlobalSnapshot: FetchEventSourceGlobalSnapshot | null = null;
let fetchEventSourceGlobalRefs = 0;

export class SseReader {
  lastEventId: string | undefined;

  readonly #url: string;
  readonly #opts: SseReaderOptions;
  readonly #abort = new AbortController();
  readonly #listeners = new Map<string, Set<Listener>>();
  #pump: Promise<void> | null = null;
  #connections = 0;
  #connectionsWaiters: Array<() => void> = [];
  #startPromise: Promise<void> | null = null;
  #resolveStart: (() => void) | null = null;

  constructor(url: string, opts: SseReaderOptions) {
    this.#url = url;
    this.#opts = opts;
    if (opts.signal) {
      if (opts.signal.aborted) {
        this.#abort.abort();
      } else {
        opts.signal.addEventListener("abort", () => this.close(), { once: true });
      }
    }
  }

  start(): Promise<void> {
    if (!this.#startPromise) {
      this.#startPromise = new Promise((resolve) => {
        this.#resolveStart = resolve;
      });
      this.#pump = this.#run();
    }
    return this.#startPromise;
  }

  close(): void {
    this.#abort.abort();
    this.#resolveConnectionWaiters();
  }

  addEventListener(type: string, listener: Listener | null): void {
    if (!listener) {
      return;
    }
    let listeners = this.#listeners.get(type);
    if (!listeners) {
      listeners = new Set();
      this.#listeners.set(type, listeners);
    }
    listeners.add(listener);
  }

  removeEventListener(type: string, listener: Listener | null): void {
    if (!listener) {
      return;
    }
    this.#listeners.get(type)?.delete(listener);
  }

  dispatchEvent(event: Event): boolean {
    const listeners = this.#listeners.get(event.type);
    if (!listeners) {
      return true;
    }
    for (const listener of listeners) {
      if (typeof listener === "function") {
        listener.call(this, event);
      } else {
        listener.handleEvent(event);
      }
    }
    return true;
  }

  async drain(options: DrainOptions = {}): Promise<void> {
    const targetConnections = options.connections;
    if (targetConnections !== undefined) {
      while (!this.#abort.signal.aborted && this.#connections < targetConnections) {
        await new Promise<void>((resolve) => this.#connectionsWaiters.push(resolve));
      }
      this.close();
    }
    await this.#pump;
  }

  async #run(): Promise<void> {
    let attempt = 0;
    const fetchImpl = this.#opts.fetch ?? globalThis.fetch;
    const headers = headersRecord(this.#opts.headers, this.lastEventId);
    const cleanupGlobals = installFetchEventSourceGlobals();

    try {
      await fetchEventSource(this.#url, {
        fetch: fetchImpl,
        headers,
        signal: this.#abort.signal,
        openWhenHidden: true,
        onopen: async (response) => {
          if (!response.ok) {
            throw new Error(`SSE request failed with status ${response.status}.`);
          }
          if (!response.body) {
            throw new Error("SSE response body is not readable.");
          }
          attempt = 0;
          this.#connections++;
          this.#resolveConnectionWaiters();
          this.#resolveStart?.();
          this.#opts.onOpen?.();
          this.dispatchEvent(new Event("open"));
        },
        onmessage: (message) => this.#handleMessage(message),
        onclose: () => {
          if (!this.#opts.retryOnDisconnect) {
            return;
          }
          throw new Error("SSE stream disconnected; reconnecting.");
        },
        onerror: (error) => {
          if (this.#abort.signal.aborted) {
            return;
          }
          const err = asError(error);
          this.#opts.onError?.(err);
          this.dispatchEvent(new ErrorEvent("error", { error: err, message: err.message }));
          if (!this.#opts.retryOnDisconnect) {
            throw err;
          }
          attempt++;
          return backoff(attempt, this.#opts);
        },
      });
    } catch {
      // Errors are reported through onerror. A rejection means the stream is done.
    } finally {
      cleanupGlobals();
      this.#resolveStart?.();
      this.#resolveConnectionWaiters();
    }
  }

  #handleMessage(message: EventSourceMessage): void {
    if (message.id !== "") {
      this.lastEventId = message.id;
    }
    if (message.data === "" && message.event === "" && message.id === "") {
      return;
    }
    const parsed: SseReaderMessage = { data: message.data };
    if (message.id !== "") parsed.id = message.id;
    if (message.event !== "") parsed.type = message.event;
    this.#opts.onMessage(parsed);
    this.dispatchEvent(new MessageEvent("message", { data: message.data }));
  }

  #resolveConnectionWaiters(): void {
    const waiters = this.#connectionsWaiters.splice(0);
    for (const resolve of waiters) {
      resolve();
    }
  }
}

function backoff(attempt: number, opts: SseReaderOptions): number {
  const base = opts.backoffBaseMs ?? 1000;
  const max = opts.backoffMaxMs ?? 30_000;
  const jitter = opts.jitter ?? 0.3;
  const capped = Math.min(max, base * 2 ** Math.max(0, attempt - 1));
  if (jitter <= 0) {
    return capped;
  }
  return capped + Math.floor(capped * jitter * Math.random());
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function headersRecord(
  headers: Record<string, string> | undefined,
  lastEventId: string | undefined,
): Record<string, string> {
  const out = { ...(headers ?? {}) };
  if (lastEventId !== undefined) {
    out["Last-Event-ID"] = lastEventId;
  }
  return out;
}

function installFetchEventSourceGlobals(): () => void {
  if (fetchEventSourceGlobalSnapshot) {
    fetchEventSourceGlobalRefs++;
    return uninstallFetchEventSourceGlobals;
  }

  const global = globalThis as FetchEventSourceGlobal;
  const snapshot: FetchEventSourceGlobalSnapshot = {
    global,
    hadWindow: Object.hasOwn(global, "window"),
    previousWindow: global.window,
    hadDocument: Object.hasOwn(global, "document"),
    previousDocument: global.document,
  };
  if (snapshot.hadWindow && snapshot.hadDocument) {
    return () => {};
  }

  fetchEventSourceGlobalSnapshot = snapshot;
  fetchEventSourceGlobalRefs = 1;

  if (!snapshot.hadWindow) {
    Object.defineProperty(global, "window", {
      configurable: true,
      writable: true,
      value: globalThis,
    });
  }
  if (!snapshot.hadDocument) {
    Object.defineProperty(global, "document", {
      configurable: true,
      writable: true,
      value: {
        hidden: false,
        addEventListener() {},
        removeEventListener() {},
      },
    });
  }

  return uninstallFetchEventSourceGlobals;
}

function uninstallFetchEventSourceGlobals(): void {
  if (!fetchEventSourceGlobalSnapshot) {
    return;
  }
  fetchEventSourceGlobalRefs--;
  if (fetchEventSourceGlobalRefs > 0) {
    return;
  }

  const snapshot = fetchEventSourceGlobalSnapshot;
  fetchEventSourceGlobalSnapshot = null;
  fetchEventSourceGlobalRefs = 0;

  if (snapshot.hadWindow) {
    Object.defineProperty(snapshot.global, "window", {
      configurable: true,
      writable: true,
      value: snapshot.previousWindow,
    });
  } else {
    Reflect.deleteProperty(snapshot.global, "window");
  }
  if (snapshot.hadDocument) {
    Object.defineProperty(snapshot.global, "document", {
      configurable: true,
      writable: true,
      value: snapshot.previousDocument,
    });
  } else {
    Reflect.deleteProperty(snapshot.global, "document");
  }
}
