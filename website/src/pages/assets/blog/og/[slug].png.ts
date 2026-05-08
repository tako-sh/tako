import type { BlogPost, BlogPostPath } from "../../../../utils/blog-endpoints";
import { getCollection } from "astro:content";
import { blogPostPaths } from "../../../../utils/blog-endpoints";
import { renderOgImage } from "../../../../utils/blog-og";

export async function getStaticPaths(): Promise<BlogPostPath[]> {
  return blogPostPaths(await getCollection("blog"));
}

export async function GET({ props }: { props: { post: BlogPost } }): Promise<Response> {
  const post = props.post;
  const png = await renderOgImage(post.data.title, post.data.image);
  const body = png.buffer.slice(png.byteOffset, png.byteOffset + png.byteLength) as ArrayBuffer;

  return new Response(body, {
    headers: {
      "Cache-Control": "public, max-age=604800",
      "Content-Type": "image/png",
    },
  });
}
