import type { RemarkPlugin } from "@astrojs/markdown-remark";
import { visit } from "unist-util-visit";
import { TAKO_D2_THEME_MARKER, TAKO_D2_THEME_SOURCE } from "../config/d2-theme.ts";

export const remarkD2Theme: RemarkPlugin = () => (tree) => {
  visit(tree, "code", (node) => {
    if (node.lang === "d2" && !node.value.includes(TAKO_D2_THEME_MARKER)) {
      node.value = `${TAKO_D2_THEME_SOURCE}\n${node.value}`;
    }
  });
};
