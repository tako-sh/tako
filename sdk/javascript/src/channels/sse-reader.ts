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
  retryOnEnd?: boolean;
}

interface DrainOptions {
  connections?: number;
}

export class SseReader {
  lastEventId: string | undefined;

  readonly #url: string;
  readonly #opts: SseReaderOptions;
  readonly #abort = new AbortController();
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
    while (!this.#abort.signal.aborted) {
      try {
        await this.#connectOnce();
        attempt = 0;
        if (!this.#opts.retryOnEnd) {
          this.#resolveStart?.();
          return;
        }
      } catch (error) {
        const err = asError(error);
        this.#opts.onError?.(err);
        if (this.#abort.signal.aborted) {
          this.#resolveStart?.();
          return;
        }
        attempt++;
        await delay(backoff(attempt, this.#opts), this.#abort.signal);
        continue;
      }

      attempt++;
      await delay(backoff(attempt, this.#opts), this.#abort.signal);
    }
    this.#resolveStart?.();
  }

  async #connectOnce(): Promise<void> {
    const fetchImpl = this.#opts.fetch ?? globalThis.fetch;
    const headers = new Headers(this.#opts.headers);
    if (this.lastEventId !== undefined) {
      headers.set("Last-Event-ID", this.lastEventId);
    }

    const response = await fetchImpl(this.#url, {
      headers,
      signal: this.#abort.signal,
    });
    if (!response.ok) {
      throw new Error(`SSE request failed with status ${response.status}.`);
    }
    if (!response.body) {
      throw new Error("SSE response body is not readable.");
    }

    this.#connections++;
    this.#resolveConnectionWaiters();
    this.#resolveStart?.();
    this.#opts.onOpen?.();

    await this.#readBody(response.body);
  }

  async #readBody(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    const parser = new SseParser((message) => {
      if (message.id !== undefined) {
        this.lastEventId = message.id;
      }
      this.#opts.onMessage(message);
    });

    try {
      while (!this.#abort.signal.aborted) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        buffer += decoder.decode(value, { stream: true });
        const lines = buffer.split(/\r\n|\r|\n/);
        buffer = lines.pop() ?? "";
        for (const line of lines) {
          parser.line(line);
        }
      }
      buffer += decoder.decode();
      if (buffer.length > 0) {
        parser.line(buffer);
      }
      parser.end();
    } finally {
      reader.releaseLock();
    }
  }

  #resolveConnectionWaiters(): void {
    const waiters = this.#connectionsWaiters.splice(0);
    for (const resolve of waiters) {
      resolve();
    }
  }
}

class SseParser {
  #eventType: string | undefined;
  #eventId: string | undefined;
  #data: string[] = [];
  readonly #emit: (message: SseReaderMessage) => void;

  constructor(emit: (message: SseReaderMessage) => void) {
    this.#emit = emit;
  }

  line(line: string): void {
    if (line === "") {
      this.#dispatch();
      return;
    }
    if (line.startsWith(":")) {
      return;
    }

    const separator = line.indexOf(":");
    const field = separator === -1 ? line : line.slice(0, separator);
    const value =
      separator === -1 ? "" : line.slice(separator + (line[separator + 1] === " " ? 2 : 1));

    switch (field) {
      case "event":
        this.#eventType = value;
        break;
      case "data":
        this.#data.push(value);
        break;
      case "id":
        this.#eventId = value;
        break;
    }
  }

  end(): void {
    this.#dispatch();
  }

  #dispatch(): void {
    if (this.#data.length === 0) {
      this.#eventType = undefined;
      return;
    }

    const message: SseReaderMessage = {
      data: this.#data.join("\n"),
    };
    if (this.#eventId !== undefined) {
      message.id = this.#eventId;
    }
    if (this.#eventType !== undefined) {
      message.type = this.#eventType;
    }
    this.#emit(message);
    this.#data = [];
    this.#eventType = undefined;
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

function delay(ms: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}
