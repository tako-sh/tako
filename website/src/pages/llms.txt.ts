import { getCollection } from "astro:content";
import type { APIContext } from "astro";
import { parseFrontmatter } from "@astrojs/markdown-remark";
import docsConfig from "../../docs/docs.json";
import { llmsProjectSection, renderLlmsTxt, type LlmsSection } from "../utils/llms";

const docsSources = import.meta.glob<string>("/docs/**/*.md", {
  query: "?raw",
  import: "default",
  eager: true,
});

export async function GET(context: APIContext) {
  const posts = await getCollection("blog");
  const site = context.site ?? new URL("https://tako.sh");

  const docsSections: LlmsSection[] = docsConfig.navigation.groups.map((group) => ({
    title: group.group,
    links: group.pages.map((page) => {
      const source = docsSources[`/docs/${page}.md`];
      if (source === undefined) {
        throw new Error(`Docs7 navigation references missing page: ${page}`);
      }
      const { frontmatter } = parseFrontmatter(source);
      const title: unknown = frontmatter["title"];
      const description: unknown = frontmatter["description"];
      if (typeof title !== "string" || !title.trim()) {
        throw new Error(`Docs7 page ${page} needs a title for llms.txt`);
      }
      return {
        title,
        url: page === "index" ? "/docs/" : `/docs/${page}/`,
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

  return new Response(renderLlmsTxt(docsConfig.name, sections), {
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
    },
  });
}
