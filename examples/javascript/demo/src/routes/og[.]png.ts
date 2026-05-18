import { createFileRoute } from "@tanstack/react-router";

import { renderOgPng } from "@/lib/og";

export const Route = createFileRoute("/og.png")({
  server: {
    handlers: {
      GET: async ({ request }) => {
        const url = new URL(request.url);
        const baseSlug = url.searchParams.get("base");
        const png = await renderOgPng({ baseSlug });
        return new Response(new Uint8Array(png), {
          headers: {
            "Content-Type": "image/png",
            "Cache-Control": "public, max-age=3600, s-maxage=86400",
          },
        });
      },
    },
  },
});
