import {
  context7AskNote,
  context7MarkdownNote,
  context7SearchLink,
  context7SearchSection,
  context7VerificationTag,
} from "./config/context7-ask.ts";

const DOCS_ORIGIN = "https://tako-sh.docs7.io";

export default {
  async fetch(request, env) {
    const incoming = new URL(request.url);
    if (incoming.pathname !== "/docs" && !incoming.pathname.startsWith("/docs/")) {
      return env.ASSETS.fetch(request);
    }

    const upstream = new URL(incoming.pathname + incoming.search, DOCS_ORIGIN);
    const headers = new Headers(request.headers);
    headers.set("Host", upstream.host);

    const upstreamResponse = await fetch(
      new Request(upstream, {
        method: request.method,
        headers,
        body: request.body,
        redirect: "manual",
      }),
    );

    const response = new Response(upstreamResponse.body, upstreamResponse);
    response.headers.append("Link", context7SearchLink);
    if (request.method === "HEAD") return response;

    const contentType = response.headers.get("Content-Type") ?? "";

    if (contentType.startsWith("text/html")) {
      return new HTMLRewriter()
        .on("head", {
          element(element) {
            element.append(context7VerificationTag, { html: true });
          },
        })
        .on("body", {
          element(element) {
            element.prepend(context7AskNote, { html: true });
          },
        })
        .transform(response);
    }

    const isMarkdown = contentType.startsWith("text/markdown");
    const isLlmsIndex =
      contentType.startsWith("text/plain") && /\/llms(?:-full)?\.txt$/.test(incoming.pathname);
    if (isMarkdown || isLlmsIndex) {
      const text = await response.text();
      const body = isMarkdown
        ? context7MarkdownNote + text
        : text.replace(/\n(?=## )/, `\n${context7SearchSection}\n`);
      response.headers.delete("Content-Length");
      if (isMarkdown) {
        response.headers.set("x-markdown-tokens", String(Math.ceil(body.length / 4)));
      }
      return new Response(body, response);
    }

    return response;
  },
} satisfies ExportedHandler<Env>;
