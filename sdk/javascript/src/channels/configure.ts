type TokenResolver = () => string | null | Promise<string | null>;

interface ChannelsConfig {
  token: TokenResolver | null;
  fetch: typeof fetch;
  websocket: typeof WebSocket;
}

function defaultFetch(): typeof fetch {
  if (typeof globalThis.fetch !== "function") {
    throw new Error("fetch is not available; pass configureChannels({ fetch }).");
  }
  return globalThis.fetch.bind(globalThis);
}

function defaultWebSocket(): typeof WebSocket {
  if (typeof globalThis.WebSocket !== "function") {
    throw new Error("WebSocket is not available; pass configureChannels({ websocket }).");
  }
  return globalThis.WebSocket;
}

function makeDefaultConfig(): ChannelsConfig {
  return {
    token: null,
    fetch: defaultFetch(),
    websocket: defaultWebSocket(),
  };
}

let current: ChannelsConfig | null = null;

function getCurrent(): ChannelsConfig {
  current ??= makeDefaultConfig();
  return current;
}

/**
 * Override browser/runtime dependencies used by channel clients.
 *
 * Use this outside a normal browser environment, or to provide an auth token
 * resolver for header-auth channels.
 *
 * @param input - Partial channel client configuration.
 * @defaultValue input.fetch = globalThis.fetch
 * @defaultValue input.websocket = globalThis.WebSocket
 */
export function configureChannels(
  input: Partial<{
    /** Token used for channel auth. Null/empty means no auth envelope. */
    token: TokenResolver;
    /** Fetch implementation used by SSE subscriptions and HTTP calls. */
    fetch: typeof fetch;
    /** WebSocket constructor used by WebSocket channel connections. */
    websocket: typeof WebSocket;
  }>,
): void {
  current = {
    ...getCurrent(),
    ...input,
  };
}

/**
 * Reset channel configuration to runtime globals.
 *
 * @internal Used by tests.
 */
export function resetChannelsConfig(): void {
  current = null;
}

/**
 * Read the effective channel client configuration.
 *
 * @internal Used by channel internals.
 */
export function getChannelsConfig(): {
  /** Fetch implementation for channel HTTP/SSE work. */
  fetch: typeof fetch;
  /** WebSocket constructor for live WebSocket channels. */
  websocket: typeof WebSocket;
  /** Resolve a required auth token or throw if none is configured. */
  resolveToken(): Promise<string>;
  /** Resolve an optional auth token. */
  resolveOptionalToken(): Promise<string | null>;
} {
  const config = getCurrent();
  return {
    fetch: config.fetch,
    websocket: config.websocket,
    async resolveToken() {
      if (!config.token) {
        throw new Error("configureChannels({ token }) is required for header-auth channels.");
      }
      const token = await config.token();
      if (!token) {
        throw new Error("configureChannels token resolver returned null.");
      }
      return token;
    },
    async resolveOptionalToken() {
      if (!config.token) return null;
      const token = await config.token();
      return token || null;
    },
  };
}
