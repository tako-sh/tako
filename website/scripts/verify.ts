import assert from "node:assert/strict";
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

const headers = await readFile(new URL("_headers", dist), "utf8");
assert.ok(headers.includes(context7SearchLink), "Search response header");
console.log(`Verified ${htmlFiles.length} HTML pages and ${markdownFiles.length} Markdown pages.`);
