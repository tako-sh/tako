import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
import { z } from "astro/zod";

const blog = defineCollection({
  loader: glob({ base: "./src/content/blog", pattern: "**/*.md" }),
  schema: z.object({
    title: z.string(),
    seoTitle: z.string().optional(),
    date: z.string(),
    description: z.string(),
    author: z.string().optional(),
    image: z
      .string()
      .nullable()
      .optional()
      .transform((v) => v || undefined),
    imageAlt: z.string().optional(),
  }),
});

export const collections = { blog };
