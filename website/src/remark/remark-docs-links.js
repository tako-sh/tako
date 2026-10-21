import { fileURLToPath } from "node:url";

const docsDirectory = fileURLToPath(new URL("../pages/docs/", import.meta.url));

export function remarkDocsLinks() {
  return function transformer(tree, file) {
    if (file.history[0].startsWith(docsDirectory)) {
      prefixLinks(tree);
    }
  };
}

function prefixLinks(node) {
  if (node.type === "link" && node.url.startsWith("/") && !node.url.startsWith("//")) {
    node.url = `/docs${node.url}`;
  }

  for (const child of node.children ?? []) {
    prefixLinks(child);
  }
}
