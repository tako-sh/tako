import { getCollection } from "astro:content";
import type { APIContext } from "astro";
import { parseFrontmatter } from "@astrojs/markdown-remark";
import { docsNavigation } from "../config/docs-navigation.ts";
import { llmsProjectSection, renderLlmsTxt, type LlmsSection } from "../utils/llms";

const docsSources = import.meta.glob<string>("/docs/**/*.{md,mdx}", {
  query: "?raw",
  import: "default",
  eager: true,
});

export async function GET(context: APIContext) {
  const posts = await getCollection("blog");
  const site = context.site ?? new URL("https://tako.sh");

  const docsSections: LlmsSection[] = docsNavigation.map((group) => ({
    title: group.title,
    links: group.pages.map((page) => {
      const source = docsSources[`/docs/${page}.md`] ?? docsSources[`/docs/${page}.mdx`];
      if (source === undefined) {
        throw new Error(`Documentation navigation references missing page: ${page}`);
      }
      const { frontmatter } = parseFrontmatter(source);
      const title: unknown = frontmatter["title"];
      const description: unknown = frontmatter["description"];
      if (typeof title !== "string" || !title.trim()) {
        throw new Error(`Documentation page ${page} needs a title for llms.txt`);
      }
      return {
        title,
        url: `/docs/${page}.md`,
        ...(typeof description === "string" ? { description } : {}),
      };
    }),
  }));

  const coreSections = [...docsSections, llmsProjectSection].map((section) => ({
    ...section,
    links: section.links.map((link) => ({
      ...link,
      url: new URL(link.url, site).toString(),
    })),
  }));

  const optionalLinks = posts
    .filter((post) => post.data.title && post.data.date)
    .sort((a, b) => new Date(b.data.date).getTime() - new Date(a.data.date).getTime())
    .map((post) => ({
      title: post.data.title,
      url: new URL(`/blog/${post.id}.md`, site).toString(),
      description: post.data.description,
    }));

  const sections: LlmsSection[] =
    optionalLinks.length > 0
      ? [...coreSections, { title: "Optional", links: optionalLinks }]
      : coreSections;

  return new Response(renderLlmsTxt("Tako", sections), {
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
    },
  });
}
