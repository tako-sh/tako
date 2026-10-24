import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { parseFrontmatter } from "@astrojs/markdown-remark";
import docsConfig from "../docs/docs.json";
import { readFile, readdir } from "node:fs/promises";
import {
  context7AskNote,
  context7SearchLink,
  context7VerificationTag,
} from "../src/config/context7-ask";

const dist = new URL("../dist/", import.meta.url);
const files = await readdir(dist, { recursive: true });
const htmlFiles = files.filter((file) => file.endsWith(".html"));
const markdownFiles = files.filter(
  (file) => file.endsWith(".md") && !file.startsWith(".well-known/"),
);
assert.ok(htmlFiles.length > 0, "Build the website before verifying it.");

for (const file of htmlFiles) {
  const html = await readFile(new URL(file, dist), "utf8");
  assert.ok(html.includes(context7VerificationTag), `${file}: verification tag`);
  assert.ok(html.includes(context7AskNote), `${file}: hidden Ask note`);
  assert.match(
    html,
    /<body[^>]*>\s*<blockquote data-context7-ask="true"/,
    `${file}: first body element`,
  );
  assert.equal(html.match(/data-context7-ask="true"/g)?.length, 1, `${file}: one Ask note`);
}

for (const file of [...markdownFiles, "llms.txt"]) {
  const text = await readFile(new URL(file, dist), "utf8");
  assert.match(text, /## Search\n/, `${file}: Search section`);
  assert.match(text, /query=<URL-encoded question>/, `${file}: question URL`);
}

const llms = await readFile(new URL("llms.txt", dist), "utf8");
for (const group of docsConfig.navigation.groups) {
  assert.ok(llms.includes(`## ${group.group}\n`), `llms.txt: ${group.group} section`);
  for (const page of group.pages) {
    const source = await readFile(new URL(`../docs/${page}.md`, import.meta.url), "utf8");
    const { frontmatter } = parseFrontmatter(source);
    const route = page === "index" ? "/docs/" : `/docs/${page}/`;
    assert.ok(
      llms.includes(`[${frontmatter["title"]}](https://tako.sh${route})`),
      `llms.txt: Docs7 page ${page} with its current title`,
    );
  }
}

for (const file of markdownFiles.filter((file) => file.startsWith("blog/"))) {
  assert.ok(llms.includes(`(https://tako.sh/${file})`), `llms.txt: blog Markdown ${file}`);
}

const skillIndex: { skills: Array<{ name: string; sha256: string; url: string }> } = JSON.parse(
  await readFile(new URL(".well-known/agent-skills/index.json", dist), "utf8"),
);
const canonicalSkills = await readdir(new URL("../../sdk/javascript/skills/", import.meta.url), {
  withFileTypes: true,
});
assert.deepEqual(
  skillIndex.skills.map((skill) => skill.name).sort(),
  canonicalSkills
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort(),
  "Skill discovery index includes every canonical skill exactly once",
);
const skillsMarkdown = await readFile(new URL("skills.md", dist), "utf8");
for (const skill of skillIndex.skills) {
  const source = await readFile(
    new URL(`../../sdk/javascript/skills/${skill.name}/SKILL.md`, import.meta.url),
    "utf8",
  );
  const published = await readFile(
    new URL(`.well-known/agent-skills/${skill.name}/SKILL.md`, dist),
    "utf8",
  );
  assert.equal(published, source, `${skill.name}: published skill matches canonical source`);
  assert.ok(
    skillsMarkdown.includes(`[${skill.name}](${skill.url})`),
    `${skill.name}: Markdown discovery link matches the structured index`,
  );
  assert.equal(
    createHash("sha256").update(published).digest("hex"),
    skill.sha256,
    `${skill.name}: discovery hash`,
  );
}

const headers = await readFile(new URL("_headers", dist), "utf8");
assert.ok(headers.includes(context7SearchLink), "Search response header");
console.log(`Verified ${htmlFiles.length} HTML pages and ${markdownFiles.length} Markdown pages.`);
