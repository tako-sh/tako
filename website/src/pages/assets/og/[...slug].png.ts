import type { PageOgImage } from "../../../data/page-og";
import { pageOgImages } from "../../../data/page-og";
import { renderPageOgImage } from "../../../utils/page-og";

export function getStaticPaths(): Array<{
  params: { slug: string };
  props: { page: PageOgImage };
}> {
  return pageOgImages.map((page) => ({
    params: { slug: page.slug },
    props: { page },
  }));
}

export async function GET({ props }: { props: { page: PageOgImage } }): Promise<Response> {
  const png = await renderPageOgImage(props.page);
  const body = png.buffer.slice(png.byteOffset, png.byteOffset + png.byteLength) as ArrayBuffer;

  return new Response(body, {
    headers: {
      "Cache-Control": "public, max-age=604800",
      "Content-Type": "image/png",
    },
  });
}
