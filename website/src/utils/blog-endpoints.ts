import type { CollectionEntry } from "astro:content";

export type BlogPost = CollectionEntry<"blog">;

const SITE = "https://tako.sh";

export interface BlogPostPath {
  params: { slug: string };
  props: { post: BlogPost };
}

export function blogPostPaths(posts: BlogPost[]): BlogPostPath[] {
  return posts.map((post) => ({
    params: { slug: post.id },
    props: { post },
  }));
}

export function blogPostCanonical(post: BlogPost): string {
  return `${SITE}/blog/${post.id}/`;
}

export function estimateMarkdownTokens(markdown: string): number {
  return Math.max(1, Math.ceil(markdown.length / 4));
}

export function blogPostBody(post: BlogPost): string {
  return (post.body ?? "").trim();
}

function yamlString(value: string): string {
  return `"${value.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`;
}

export function blogPostMarkdown(post: BlogPost): string {
  const lines = [
    "---",
    `title: ${yamlString(post.data.title)}`,
    `date: ${yamlString(post.data.date)}`,
    `description: ${yamlString(post.data.description)}`,
  ];

  if (post.data.author) lines.push(`author: ${yamlString(post.data.author)}`);
  if (post.data.image) lines.push(`image: ${yamlString(post.data.image)}`);
  if (post.data.imageAlt) lines.push(`imageAlt: ${yamlString(post.data.imageAlt)}`);

  lines.push(`canonical: ${yamlString(blogPostCanonical(post))}`);
  lines.push("---", "", blogPostBody(post), "");
  return lines.join("\n");
}
