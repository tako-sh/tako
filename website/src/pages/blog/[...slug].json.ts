import { getCollection, render } from "astro:content";
import {
  blogPostBody,
  blogPostCanonical,
  blogPostPaths,
  type BlogPost,
  type BlogPostPath,
} from "../../utils/blog-endpoints";

export async function getStaticPaths(): Promise<BlogPostPath[]> {
  return blogPostPaths(await getCollection("blog"));
}

export async function GET({ props }: { props: { post: BlogPost } }): Promise<Response> {
  const post = props.post;
  const { headings } = await render(post);

  return Response.json({
    slug: post.id,
    url: blogPostCanonical(post),
    canonical: blogPostCanonical(post),
    title: post.data.title,
    date: post.data.date,
    description: post.data.description,
    author: post.data.author ?? null,
    image: post.data.image ?? null,
    imageAlt: post.data.imageAlt ?? null,
    headings,
    markdown: blogPostBody(post),
  });
}
