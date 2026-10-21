const DOCS_ORIGIN = "https://tako-sh.docs7.io";

export default {
  fetch(request, env) {
    const incoming = new URL(request.url);
    if (incoming.pathname !== "/docs" && !incoming.pathname.startsWith("/docs/")) {
      return env.ASSETS.fetch(request);
    }

    const upstream = new URL(incoming.pathname + incoming.search, DOCS_ORIGIN);
    const headers = new Headers(request.headers);
    headers.set("Host", upstream.host);

    return fetch(
      new Request(upstream, {
        method: request.method,
        headers,
        body: request.body,
        redirect: "manual",
      }),
    );
  },
};
