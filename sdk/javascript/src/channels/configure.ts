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

export function configureChannels(
  input: Partial<{
    token: TokenResolver;
    fetch: typeof fetch;
    websocket: typeof WebSocket;
  }>,
): void {
  current = {
    ...getCurrent(),
    ...input,
  };
}

export function resetChannelsConfig(): void {
  current = null;
}

export function getChannelsConfig(): {
  fetch: typeof fetch;
  websocket: typeof WebSocket;
  resolveToken(): Promise<string>;
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
  };
}
