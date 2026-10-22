import type { ShikiConfig } from "@astrojs/markdown-remark";

export const SNIPPET_THEME = {
  name: "tako-ink",
  type: "dark",
  colors: {
    "editor.background": "#2f2a44",
    "editor.foreground": "#efebe6",
    "editorLineNumber.foreground": "#a39db0",
    "editor.selectionBackground": "#4a4362",
    "editorCursor.foreground": "#e88783",
    "editorIndentGuide.background": "#3d3756",
  },
  tokenColors: [
    {
      scope: ["comment", "punctuation.definition.comment"],
      settings: {
        foreground: "#a39db0",
        fontStyle: "italic",
      },
    },
    {
      scope: ["keyword", "storage", "storage.type"],
      settings: {
        foreground: "#e88783",
      },
    },
    {
      scope: ["string", "string.quoted", "string.template"],
      settings: {
        foreground: "#9bc4b6",
      },
    },
    {
      scope: ["constant.numeric", "constant.character", "constant.language"],
      settings: {
        foreground: "#d6a63e",
      },
    },
    {
      scope: ["entity.name.function", "support.function", "meta.function-call"],
      settings: {
        foreground: "#efebe6",
      },
    },
    {
      scope: ["entity.name.type", "support.type", "storage.type.class"],
      settings: {
        foreground: "#d6a63e",
      },
    },
    {
      scope: ["operator", "keyword.operator"],
      settings: {
        foreground: "#b8b2c2",
      },
    },
    {
      scope: ["variable", "identifier"],
      settings: {
        foreground: "#efebe6",
      },
    },
    {
      scope: ["punctuation", "meta.brace", "delimiter"],
      settings: {
        foreground: "#b8b2c2",
      },
    },
    {
      scope: ["invalid"],
      settings: {
        foreground: "#1f1b2b",
        background: "#e88783",
      },
    },
  ],
} satisfies ShikiConfig["theme"];
