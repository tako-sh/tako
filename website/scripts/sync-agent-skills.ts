#!/usr/bin/env bun
// Syncs canonical SDK skills into repo discovery locations and the website,
// and writes an index.json per the Agent Skills Discovery RFC v0.2.0.
//
// Runs before astro build so the generated files are picked up as static assets.

import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseFrontmatter } from "@astrojs/markdown-remark";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(fileURLToPath(new URL(".", import.meta.url)), "..");
const REPO_ROOT = path.resolve(ROOT, "..");
const SRC_SKILLS = path.join(REPO_ROOT, "sdk/javascript/skills");
const DEST_DIR = path.join(ROOT, "public/.well-known/agent-skills");
const SITE = "https://tako.sh";

async function main() {
  const entries = await readdir(SRC_SKILLS, { withFileTypes: true });
  const skills: Array<{
    name: string;
    type: string;
    description: string;
    url: string;
    sha256: string;
  }> = [];

  await mkdir(DEST_DIR, { recursive: true });

  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    const slug = entry.name;
    const srcPath = path.join(SRC_SKILLS, slug, "SKILL.md");
    const content = await readFile(srcPath, "utf8");
    const { frontmatter } = parseFrontmatter(content);
    const description: unknown = frontmatter["description"];
    const type: unknown = frontmatter["type"];
    for (const directory of [
      DEST_DIR,
      path.join(REPO_ROOT, "skills"),
      path.join(REPO_ROOT, ".agents/skills"),
    ]) {
      const destDir = path.join(directory, slug);
      await mkdir(destDir, { recursive: true });
      await writeFile(path.join(destDir, "SKILL.md"), content);
    }
    const sha256 = createHash("sha256").update(content).digest("hex");
    skills.push({
      name: slug,
      type: typeof type === "string" ? type : "framework",
      description: typeof description === "string" ? description : "",
      url: `${SITE}/.well-known/agent-skills/${slug}/SKILL.md`,
      sha256,
    });
  }
  skills.sort((a, b) => a.name.localeCompare(b.name));

  const index = {
    $schema: "https://agentskills.io/schemas/v0.2.0/index.json",
    skills,
  };
  await writeFile(path.join(DEST_DIR, "index.json"), JSON.stringify(index, null, 2) + "\n");
  console.log(`sync-agent-skills: wrote ${skills.length} skill(s) + index.json`);
}

await main();
