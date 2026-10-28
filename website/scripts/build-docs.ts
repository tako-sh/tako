import { cp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { existsSync } from "node:fs";
import { spawn } from "bun";
import { siteUrl } from "../src/config/site-identity.ts";
import { llmsSummary } from "../src/utils/llms.ts";
import {
  context7AskNote,
  context7MarkdownNote,
  context7SearchSection,
  context7VerificationTag,
} from "../src/config/context7-ask.ts";

const project = new URL("../documentation/", import.meta.url);
const source = new URL("dist/", project);
const destination = new URL("../dist/docs/", import.meta.url);

const build = spawn(["bun", "x", "--no-install", "blume", "build"], {
  cwd: fileURLToPath(project),
  stdout: "inherit",
  stderr: "inherit",
});
const exitCode = await build.exited;
if (exitCode !== 0) process.exit(exitCode);

// Blume's deployment base rewrites URLs; the combined site supplies the directory mount.
await rm(destination, { recursive: true, force: true });
await mkdir(destination, { recursive: true });
await cp(source, destination, {
  recursive: true,
  filter: (path) =>
    !["_headers", "_redirects", "vercel.json"].some(
      (name) => path === fileURLToPath(new URL(name, source)),
    ),
});

for (const file of await readdir(destination, { recursive: true })) {
  const url = new URL(file, destination);
  if (file.endsWith(".html")) {
    const html = await readFile(url, "utf8");
    await writeFile(
      url,
      html
        .replace(
          /<head\b[^>]*>/,
          `$&${context7VerificationTag}<link rel="alternate" type="application/rss+xml" title="Tako Blog" href="/rss.xml">`,
        )
        .replace(/<body\b[^>]*>/, `$&${context7AskNote}`),
    );
  } else if (/\.mdx?$/.test(file) && !file.startsWith(".well-known/")) {
    const text = await readFile(url, "utf8");
    const frontmatter = text.match(/^---\r?\n[\s\S]*?\r?\n---\r?\n/);
    const offset = frontmatter?.[0].length ?? 0;
    await writeFile(url, text.slice(0, offset) + "\n" + context7MarkdownNote + text.slice(offset));
  } else if (/^llms(?:-full)?\.txt$/.test(file)) {
    const text = await readFile(url, "utf8");
    await writeFile(url, text.replace(/\n(?=## )/, `\n${context7SearchSection}\n`));
  }
}

// Cloudflare reads these only at the deployment root. Replace our section on rebuilds.
async function mergeRules(name: string): Promise<void> {
  const output = new URL(`../${name}`, destination);
  const marker = "\n# Blume documentation\n";
  const existing = await readFile(output, "utf8");
  let rules = await readFile(new URL(name, source), "utf8");
  if (name === "_headers") {
    // Cloudflare combines matching headers; the general Markdown rule already covers skills.
    rules = rules.replace(
      "/docs/.well-known/agent-skills/*.md\n  Content-Type: text/markdown; charset=utf-8\n",
      "",
    );
  }
  await writeFile(output, existing.split(marker)[0]!.trimEnd() + "\n" + marker + rules);
}
await mergeRules("_headers");
const redirects = new URL("_redirects", source);
if (existsSync(redirects)) {
  await mergeRules("_redirects");
}

// Domain-level discovery starts at /.well-known, even when the docs live under /docs.
const site = new URL("../", destination);
await mkdir(new URL(".well-known/", site), { recursive: true });
for (const name of ["api-catalog", "ai-catalog.json", "ard.json"]) {
  await cp(new URL(`.well-known/${name}`, destination), new URL(`.well-known/${name}`, site));
}
const manifest = JSON.parse(await readFile(new URL("agent-readability.json", destination), "utf8"));
const rootManifest = {
  ...manifest,
  description: llmsSummary,
  artifacts: {
    ...manifest.artifacts,
    llmsTxt: `${siteUrl}/llms.txt`,
    aiCatalog: `${siteUrl}/.well-known/ai-catalog.json`,
    apiCatalog: `${siteUrl}/.well-known/api-catalog`,
    agentSkills: `${siteUrl}/.well-known/agent-skills/index.json`,
    httpMessageSignaturesDirectory: `${siteUrl}/.well-known/http-message-signatures-directory`,
    sitemap: `${siteUrl}/sitemap-index.xml`,
    feeds: [`${siteUrl}/rss.xml`],
  },
};
await writeFile(
  new URL("agent-readability.json", site),
  JSON.stringify(rootManifest, null, 2) + "\n",
);
console.log("Published Blume documentation to website/dist/docs.");
