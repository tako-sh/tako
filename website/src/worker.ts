// Cloudflare Worker entry. Static Astro assets are bound as `ASSETS`; the
// Worker only intercepts GET requests that prefer markdown via the Accept
// header (RFC 9110 content negotiation) and rewrites to the sibling .md file
// emitted at build time by scripts/emit-markdown.ts.

import { normalizeCanonicalPath } from "./utils/canonical";

interface Env {
  ASSETS: Fetcher;
}

function canonicalRedirect(request: Request, url: URL): Response | null {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return null;
  }

  const canonicalPathname = normalizeCanonicalPath(url.pathname);
  if (canonicalPathname === url.pathname) {
    return null;
  }

  const redirectUrl = new URL(url);
  redirectUrl.pathname = canonicalPathname;
  return Response.redirect(redirectUrl, 301);
}

function prefersMarkdown(accept: string | null): boolean {
  if (!accept) return false;
  for (const part of accept.split(",")) {
    const [type] = part.trim().split(";");
    if (type && type.trim().toLowerCase() === "text/markdown") return true;
  }
  return false;
}

function markdownPath(pathname: string): string {
  if (pathname === "" || pathname === "/") return "/index.md";
  const trimmed = pathname.endsWith("/") ? pathname.slice(0, -1) : pathname;
  return `${trimmed}/index.md`;
}

// Fast approximation: ~4 characters per token. Good enough for agent
// budgeting and cheap to compute at the edge.
function estimateTokens(text: string): number {
  return Math.max(1, Math.ceil(text.length / 4));
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const redirect = canonicalRedirect(request, url);
    if (redirect) {
      return redirect;
    }

    if (
      (request.method === "GET" || request.method === "HEAD") &&
      prefersMarkdown(request.headers.get("accept"))
    ) {
      const mdUrl = new URL(url);
      mdUrl.pathname = markdownPath(url.pathname);
      const mdResponse = await env.ASSETS.fetch(new Request(mdUrl.toString(), request));
      if (mdResponse.ok) {
        const text = await mdResponse.text();
        const headers = new Headers(mdResponse.headers);
        headers.set("Content-Type", "text/markdown; charset=utf-8");
        headers.set("x-markdown-tokens", String(estimateTokens(text)));
        const existingVary = headers.get("Vary");
        headers.set("Vary", existingVary ? `${existingVary}, Accept` : "Accept");
        return new Response(text, { status: 200, headers });
      }
    }

    return env.ASSETS.fetch(request);
  },
} satisfies ExportedHandler<Env>;
