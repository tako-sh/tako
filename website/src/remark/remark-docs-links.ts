import { fileURLToPath } from "node:url";
import type { RemarkPlugin } from "@astrojs/markdown-remark";
import { visit } from "unist-util-visit";

const docsDirectory = fileURLToPath(new URL("../pages/docs/", import.meta.url));

export const remarkDocsLinks: RemarkPlugin = () => (tree, file) => {
  if (!file.history[0]?.startsWith(docsDirectory)) return;

  visit(tree, "link", (node) => {
    if (node.url.startsWith("/") && !node.url.startsWith("//")) {
      node.url = `/docs${node.url}`;
    }
  });
};
