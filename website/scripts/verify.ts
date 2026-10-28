import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { parseFrontmatter } from "@astrojs/markdown-remark";
import { docsNavigation, docsRoute } from "../src/config/docs-navigation.ts";
import { siteEntityIds, siteUrl } from "../src/config/site-identity.ts";
import { readFile, readdir } from "node:fs/promises";
import {
  context7AskNote,
  context7SearchLink,
  context7VerificationTag,
} from "../src/config/context7-ask";

const dist = new URL("../dist/", import.meta.url);
const files = await readdir(dist, { recursive: true });
const docsSources = await readdir(new URL("../docs/", import.meta.url));
const htmlFiles = files.filter((file) => file.endsWith(".html"));
const markdownFiles = files.filter(
  (file) => /\.mdx?$/.test(file) && !/(^|\/)\.well-known\//.test(file),
);
assert.ok(htmlFiles.length > 0, "Build the website before verifying it.");

function structuredData(html: string): Array<Record<string, unknown>> {
  return [...html.matchAll(/<script[^>]*type="application\/ld\+json"[^>]*>(.*?)<\/script>/gs)]
    .flatMap((match) => JSON.parse(match[1]!))
    .flatMap((node) => node["@graph"] ?? node);
}

async function readJson(file: string) {
  return JSON.parse(await readFile(new URL(file, dist), "utf8"));
}

async function documentHead(html: string) {
  let title = "";
  const metadata = new Map<string, string[]>();
  const links: Array<{ rel: string | null; href: string | null; type: string | null }> = [];
  await new HTMLRewriter()
    .on("head title", {
      text(chunk) {
        title += chunk.text;
      },
    })
    .on("head meta", {
      element(element) {
        const key = element.getAttribute("property") ?? element.getAttribute("name");
        if (key) {
          const values = metadata.get(key) ?? [];
          values.push(element.getAttribute("content") ?? "");
          metadata.set(key, values);
        }
      },
    })
    .on("head link", {
      element(element) {
        links.push({
          rel: element.getAttribute("rel"),
          href: element.getAttribute("href"),
          type: element.getAttribute("type"),
        });
      },
    })
    .transform(new Response(html))
    .text();
  return { title: title.trim(), metadata, links };
}

const imageFiles = new Set<string>();
const iconFiles = new Map<string, string>();
const heads = new Map<string, Awaited<ReturnType<typeof documentHead>>>();
const homeSchema = structuredData(await readFile(new URL("index.html", dist), "utf8"));
for (const [type, id] of [
  ["Organization", siteEntityIds.organization],
  ["WebSite", siteEntityIds.website],
  ["SoftwareApplication", siteEntityIds.software],
]) {
  assert.equal(homeSchema.filter((node) => node["@type"] === type && node["@id"] === id).length, 1);
}
const pagesIndex = await readJson("docs/api/docs/pages.json");
assert.equal(pagesIndex.count, docsNavigation.flatMap((group) => group.pages).length);

for (const file of htmlFiles) {
  const html = await readFile(new URL(file, dist), "utf8");
  const head = await documentHead(html);
  heads.set(file, head);
  const meta = (key: string): string => {
    const values = head.metadata.get(key) ?? [];
    assert.equal(values.length, 1, `${file}: exactly one ${key}`);
    assert.ok(values[0], `${file}: nonempty ${key}`);
    return values[0]!;
  };
  for (const [rel, asset, type] of [
    ["icon", "logo.svg", "image/svg+xml"],
    ["apple-touch-icon", "apple-touch-icon.png", undefined],
  ] as const) {
    const links = head.links.filter((link) => link.rel === rel);
    assert.equal(links.length, 1, `${file}: one shared ${rel}`);
    const href = links[0]!.href;
    const expected = file.startsWith("docs/")
      ? `/docs/${rel === "icon" ? "icon.svg" : asset}`
      : `/assets/${asset}`;
    assert.equal(href, expected, `${file}: Tako ${rel}, without the Blume fallback`);
    if (type) assert.equal(links[0]!.type, type, `${file}: ${rel} MIME type`);
    iconFiles.set(expected.slice(1), asset);
  }
  assert.equal(meta("theme-color").toLowerCase(), "#1f1b2b", `${file}: brand theme color`);
  if (!/(^|\/)404\.html$/.test(file)) {
    const canonical = head.links.filter((link) => link.rel === "canonical");
    assert.equal(canonical.length, 1, `${file}: one canonical URL`);
    assert.ok(canonical[0]!.href?.startsWith(`${siteUrl}/`), `${file}: production canonical`);
    assert.equal(meta("og:url"), canonical[0]!.href, `${file}: OG canonical URL`);
    assert.equal(meta("og:site_name"), "Tako", `${file}: OG site name`);
    assert.ok(["website", "article"].includes(meta("og:type")), `${file}: OG type`);
    assert.equal(meta("og:title"), head.title, `${file}: OG title`);
    assert.equal(meta("twitter:title"), head.title, `${file}: Twitter title`);
    assert.equal(meta("og:description"), meta("description"), `${file}: OG description`);
    assert.equal(meta("twitter:description"), meta("description"), `${file}: Twitter description`);
    assert.equal(meta("twitter:card"), "summary_large_image", `${file}: social preview card`);
    assert.equal(meta("twitter:site"), "@lilienblum", `${file}: Twitter account`);
    const image = new URL(meta("og:image"));
    assert.equal(image.origin, siteUrl, `${file}: locally built OG image`);
    assert.equal(meta("twitter:image"), image.href, `${file}: shared social image`);
    assert.equal(meta("og:image:type"), "image/png", `${file}: OG image type`);
    assert.equal(meta("og:image:width"), "1200", `${file}: OG image width`);
    assert.equal(meta("og:image:height"), "630", `${file}: OG image height`);
    assert.equal(meta("twitter:image:alt"), meta("og:image:alt"), `${file}: social image alt`);
    imageFiles.add(image.pathname.slice(1));
  }
  assert.ok(html.includes(context7VerificationTag), `${file}: verification tag`);
  assert.ok(html.includes(context7AskNote), `${file}: hidden Ask note`);
  assert.match(
    html,
    /<body[^>]*>\s*<blockquote data-context7-ask="true"/,
    `${file}: first body element`,
  );
  assert.equal(html.match(/data-context7-ask="true"/g)?.length, 1, `${file}: one Ask note`);
}

for (const [file, source] of iconFiles) {
  assert.deepEqual(
    await readFile(new URL(file, dist)),
    await readFile(new URL(`../public/assets/${source}`, import.meta.url)),
    `${file}: same icon artwork as the main site`,
  );
}
const appleIcon = await readFile(new URL("assets/apple-touch-icon.png", dist));
assert.equal(appleIcon.readUInt32BE(16), 180, "Apple touch icon width");
assert.equal(appleIcon.readUInt32BE(20), 180, "Apple touch icon height");
for (const file of imageFiles) {
  const image = await readFile(new URL(file, dist));
  assert.equal(image.subarray(0, 8).toString("hex"), "89504e470d0a1a0a", `${file}: valid PNG`);
  assert.equal(image.readUInt32BE(16), 1200, `${file}: actual social image width`);
  assert.equal(image.readUInt32BE(20), 630, `${file}: actual social image height`);
}

for (const file of [...markdownFiles, "llms.txt", "docs/llms.txt", "docs/llms-full.txt"]) {
  const text = await readFile(new URL(file, dist), "utf8");
  assert.match(text, /## Search\n/, `${file}: Search section`);
  assert.match(text, /query=<URL-encoded question>/, `${file}: question URL`);
}

const llms = await readFile(new URL("llms.txt", dist), "utf8");
const docsLlms = await readFile(new URL("docs/llms.txt", dist), "utf8");
const docsSitemap = await readFile(new URL("docs/sitemap.xml", dist), "utf8");
const docsSearch: Array<{ route: string; title: string }> = JSON.parse(
  await readFile(new URL("docs/blume-search.json", dist), "utf8"),
);
for (const group of docsNavigation) {
  assert.ok(llms.includes(`## ${group.title}\n`), `llms.txt: ${group.title} section`);
  for (const page of group.pages) {
    const sourceFile = docsSources.find((file) => file === `${page}.md` || file === `${page}.mdx`);
    assert.ok(sourceFile, `${page}: authored documentation source`);
    const source = await readFile(new URL(`../docs/${sourceFile}`, import.meta.url), "utf8");
    const { frontmatter } = parseFrontmatter(source);
    const route = docsRoute(page);
    const url = `https://tako.sh${route}`;
    assert.ok(
      llms.includes(`[${frontmatter["title"]}](${siteUrl}/docs/${page}.md)`),
      `llms.txt: documentation page ${page} with its current title`,
    );
    assert.ok(docsLlms.includes(`](${url})`), `Blume index: ${page}`);
    assert.ok(docsSitemap.includes(`<loc>${url}</loc>`), `Blume sitemap: ${page}`);
    assert.ok(
      docsSearch.some(
        (entry) =>
          entry.route === (page === "index" ? "/" : `/${page}`) &&
          entry.title === frontmatter["title"],
      ),
      `Blume search: ${page}`,
    );
    const htmlFile = page === "index" ? "docs/index.html" : `docs/${page}/index.html`;
    const html = await readFile(new URL(htmlFile, dist), "utf8");
    const head = heads.get(htmlFile)!;
    assert.equal(head.title, `${frontmatter["title"]} - Tako`, `${page}: authored social title`);
    assert.deepEqual(head.metadata.get("description"), [frontmatter["description"]]);
    assert.equal(html.match(/<h1(?:\s|>)/g)?.length, 1, `${page}: one rendered page title`);
    const schema = structuredData(html);
    assert.ok(
      !schema.some((node) => node["@type"] === "WebSite"),
      `${page}: shared website identity`,
    );
    const article = schema.find((node) => node["url"] === url);
    assert.ok(article, `${page}: structured page data`);
    assert.deepEqual(article["isPartOf"], { "@id": siteEntityIds.website });
    assert.deepEqual(article["publisher"], { "@id": siteEntityIds.organization });
    // Git dates are absent for new pages until their first commit.
    for (const field of ["datePublished", "dateModified"]) {
      const value = article[field];
      if (value !== undefined) {
        assert.ok(typeof value === "string", `${page}: ${field} is a string`);
        assert.ok(Number.isFinite(Date.parse(value)), `${page}: ${field}`);
      }
    }
    assert.equal(article["image"], `${siteUrl}/docs/og/${page}.png`);
    const image = await readFile(new URL(`docs/og/${page}.png`, dist));
    assert.equal(image.readUInt32BE(16), 1200, `${page}: OG width`);
    assert.equal(image.readUInt32BE(20), 630, `${page}: OG height`);
    assert.ok(html.includes('title="Tako Blog" href="/rss.xml"'), `${page}: blog feed discovery`);
    const apiPage = await readJson(`docs/api/docs/pages/${page}.json`);
    assert.equal(apiPage.title, frontmatter["title"], `${page}: JSON API title`);
    assert.equal(apiPage.url, url, `${page}: JSON API URL`);
    assert.equal(apiPage.markdownUrl, `${siteUrl}/docs/${page}.md`);
    for (const heading of source.match(/^#{2,6} .+$/gm) ?? []) {
      assert.ok(apiPage.markdown.includes(heading), `${page}: JSON API heading ${heading}`);
    }
    assert.ok(
      pagesIndex.pages.some((entry: { url: string }) => entry.url === url),
      `${page}: JSON API index`,
    );
    assert.ok(html.includes(`rel="canonical" href="${url}"`), `${page}: canonical URL`);
    assert.equal(
      html.match(/src="https:\/\/context7.com\/widget.js"/g)?.length,
      1,
      `${page}: one Context7 widget`,
    );
    assert.ok(html.includes('data-library="/tako-sh/tako"'), `${page}: current Context7 library`);
    assert.ok(
      html.includes(`https://github.com/tako-sh/tako/edit/master/website/docs/${sourceFile}`),
      `${page}: source edit link`,
    );
    const markdown = await readFile(new URL(`docs/${page}.md`, dist), "utf8");
    if (source.includes("<CodeGroup>")) {
      assert.ok(html.includes("<blume-tabs"), `${page}: interactive code tabs`);
      assert.equal(
        html.match(/<blume-tabs(?:\s|>)/g)?.length,
        source.match(/<CodeGroup>/g)?.length,
        `${page}: every code group renders a tab set`,
      );
      assert.ok(!html.includes("<codegroup"), `${page}: no unresolved code-group markup`);
      assert.ok(!markdown.includes("<CodeGroup"), `${page}: code groups export as plain Markdown`);
      assert.ok(
        !apiPage.markdown.includes("<CodeGroup"),
        `${page}: JSON API code groups are readable`,
      );
      for (const fence of source.match(/^```\w+ [^\n]+$/gm) ?? []) {
        assert.ok(markdown.includes(fence), `${page}: preserves code variant ${fence}`);
      }
    }
    assert.equal(
      parseFrontmatter(markdown).frontmatter["title"],
      frontmatter["title"],
      `${page}: Markdown frontmatter`,
    );
    assert.ok(!html.includes("<featurerequests"), `${page}: no unresolved component`);
  }
}

assert.deepEqual(
  files
    .filter(
      (file) => /^docs\/[^/]+\.md$/.test(file) && !["docs/404.md", "docs/skill.md"].includes(file),
    )
    .sort(),
  docsNavigation.flatMap((group) => group.pages.map((page) => `docs/${page}.md`)).sort(),
  "Every documentation page appears in navigation",
);

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
const docsSkillIndex = await readJson("docs/.well-known/agent-skills/index.json");
assert.deepEqual(
  docsSkillIndex.skills.map((skill: { name: string }) => skill.name).sort(),
  skillIndex.skills.map((skill) => skill.name).sort(),
  "Blume publishes the canonical skills",
);
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
  const docsSkill = docsSkillIndex.skills.find(
    (entry: { name: string }) => entry.name === skill.name,
  );
  assert.equal(await readFile(new URL(docsSkill.url.slice(1), dist), "utf8"), source);
  assert.equal(docsSkill.digest, `sha256:${skill.sha256}`, `${skill.name}: Blume discovery hash`);
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

const openapi = await readJson("docs/openapi.json");
assert.equal(openapi.servers[0].url, `${siteUrl}/docs`);
assert.ok(
  !Object.keys(openapi.paths).some((path) => /search|mcp/.test(path)),
  "Static API capabilities",
);
for (const scope of ["", "docs/"]) {
  const manifest = await readJson(`${scope}agent-readability.json`);
  assert.ok(!manifest.artifacts.mcp && !manifest.artifacts.api.search);
  assert.ok(!manifest.artifacts.markdown.contentNegotiation);
  const urls = [
    ...Object.values(manifest.artifacts).filter((value) => typeof value === "string"),
    ...Object.values(manifest.artifacts.api),
    ...(manifest.artifacts.feeds ?? []),
  ] as string[];
  for (const url of urls) {
    assert.equal(new URL(url).origin, siteUrl);
    assert.ok(files.includes(new URL(url).pathname.slice(1)), `${scope}manifest: ${url} exists`);
  }
}
for (const name of ["api-catalog", "ai-catalog.json", "ard.json"]) {
  assert.deepEqual(
    await readJson(`.well-known/${name}`),
    await readJson(`docs/.well-known/${name}`),
  );
}
const catalog = await readJson(".well-known/ai-catalog.json");
for (const entry of catalog.entries) {
  assert.ok(
    files.includes(new URL(entry.url).pathname.slice(1)),
    `Catalog entry exists: ${entry.url}`,
  );
}
assert.ok(
  (await readFile(new URL("sitemap-index.xml", dist), "utf8")).includes(
    `${siteUrl}/docs/sitemap.xml`,
  ),
  "Root sitemap discovers documentation sitemap",
);

const headers = await readFile(new URL("_headers", dist), "utf8");
assert.ok(headers.includes(context7SearchLink), "Search response header");
assert.ok(
  headers.includes("/docs/*.md\n  Content-Type: text/markdown; charset=utf-8"),
  "Blume Markdown response headers",
);
assert.ok(headers.includes("/docs/.well-known/api-catalog"), "Blume discovery response headers");
assert.ok(
  headers.includes('</.well-known/api-catalog>; rel="api-catalog"'),
  "Root catalog discovery",
);
console.log(`Verified ${htmlFiles.length} HTML pages and ${markdownFiles.length} Markdown pages.`);
