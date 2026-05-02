import { getCollection } from "astro:content";
import {
  blogPostMarkdown,
  blogPostPaths,
  estimateMarkdownTokens,
  type BlogPost,
  type BlogPostPath,
} from "../../utils/blog-endpoints";

export async function getStaticPaths(): Promise<BlogPostPath[]> {
  return blogPostPaths(await getCollection("blog"));
}

export function GET({ props }: { props: { post: BlogPost } }): Response {
  const markdown = blogPostMarkdown(props.post);
  return new Response(markdown, {
    headers: {
      "Content-Type": "text/markdown; charset=utf-8",
      "x-markdown-tokens": String(estimateMarkdownTokens(markdown)),
    },
  });
}
