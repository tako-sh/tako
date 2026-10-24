import { createHash } from "node:crypto";
import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import { parseFrontmatter } from "@astrojs/markdown-remark";
import type { APIContext } from "astro";
import { context7MarkdownNote } from "../config/context7-ask.ts";

const cwd = process.cwd();
const REPO_ROOT = path.basename(cwd) === "website" ? path.resolve(cwd, "..") : cwd;
const SRC_SKILLS = path.join(REPO_ROOT, "sdk/javascript/skills");

interface SkillSummary {
  name: string;
  description: string;
  url: string;
  sha256: string;
}

async function readSkillSummaries(site: URL): Promise<SkillSummary[]> {
  const entries = await readdir(SRC_SKILLS, { withFileTypes: true });
  const skills: SkillSummary[] = [];

  for (const entry of entries) {
    if (!entry.isDirectory()) continue;

    const name = entry.name;
    const content = await readFile(path.join(SRC_SKILLS, name, "SKILL.md"), "utf8");
    const { frontmatter } = parseFrontmatter(content);
    const description: unknown = frontmatter["description"];

    skills.push({
      name,
      description: typeof description === "string" ? description : "",
      url: new URL(`/.well-known/agent-skills/${name}/SKILL.md`, site).toString(),
      sha256: createHash("sha256").update(content).digest("hex"),
    });
  }

  return skills.sort((a, b) => a.name.localeCompare(b.name));
}

function renderSkillsMd(site: URL, skills: SkillSummary[]): string {
  const lines = [
    context7MarkdownNote,
    "# Tako Agent Skills",
    "",
    "> Machine-readable skills for agents working with Tako.",
    "",
    `Structured discovery index: ${new URL("/.well-known/agent-skills/index.json", site).toString()}`,
    `Full site documentation index: ${new URL("/llms.txt", site).toString()}`,
    "Install skills and MCP with `tako agents`. Setup guide: https://tako.sh/docs/agents/",
    "Install with skills.sh: `npx skills add tako-sh/tako`",
    "",
    "Fetch only the `SKILL.md` file that matches the current task.",
    "",
    "## Skills",
    "",
  ];

  for (const skill of skills) {
    const suffix = skill.description ? `: ${skill.description}` : "";
    lines.push(`- [${skill.name}](${skill.url})${suffix}`);
    lines.push(`  SHA-256: \`${skill.sha256}\``);
  }

  return `${lines.join("\n").trim()}\n`;
}

export async function GET(context: APIContext): Promise<Response> {
  const site = context.site ?? new URL("https://tako.sh");
  const skills = await readSkillSummaries(site);

  return new Response(renderSkillsMd(site, skills), {
    headers: {
      "Content-Type": "text/markdown; charset=utf-8",
    },
  });
}
