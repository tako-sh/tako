export const SNIPPET_THEME = {
  name: "catppuccin-latte-readable",
  type: "light",
  colors: {
    "editor.background": "#eff1f5",
    "editor.foreground": "#4c4f69",
    "editorLineNumber.foreground": "#8c8fa1",
    "editor.selectionBackground": "#ccd0da",
    "editorCursor.foreground": "#4c4f69",
    "editorIndentGuide.background": "#dce0e8",
  },
  tokenColors: [
    {
      scope: ["comment", "punctuation.definition.comment"],
      settings: {
        foreground: "#5c5f77",
        fontStyle: "italic",
      },
    },
    {
      scope: ["keyword", "storage", "storage.type"],
      settings: {
        foreground: "#8839ef",
      },
    },
    {
      scope: ["string", "string.quoted", "string.template"],
      settings: {
        foreground: "#2b7a1f",
      },
    },
    {
      scope: ["constant.numeric", "constant.character", "constant.language"],
      settings: {
        foreground: "#a84300",
      },
    },
    {
      scope: ["entity.name.function", "support.function", "meta.function-call"],
      settings: {
        foreground: "#1c5bd5",
      },
    },
    {
      scope: ["entity.name.type", "support.type", "storage.type.class"],
      settings: {
        foreground: "#8a5600",
      },
    },
    {
      scope: ["operator", "keyword.operator"],
      settings: {
        foreground: "#0b6f76",
      },
    },
    {
      scope: ["variable", "identifier"],
      settings: {
        foreground: "#4c4f69",
      },
    },
    {
      scope: ["punctuation", "meta.brace", "delimiter"],
      settings: {
        foreground: "#5c5f77",
      },
    },
    {
      scope: ["invalid"],
      settings: {
        foreground: "#eff1f5",
        background: "#d20f39",
      },
    },
  ],
};
