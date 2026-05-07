import { generateOgImage } from "../src/utils/blog-og";

export { generateOgImage, renderOgImage } from "../src/utils/blog-og";

if (import.meta.main) {
  const [title, outputPath, imageSource] = process.argv.slice(2);
  if (!title || !outputPath) {
    console.error(
      "Usage: bun run generate-og.tsx <title> <output-path> [blog-image-id-or-source-path]",
    );
    process.exit(1);
  }
  await generateOgImage(title, outputPath, imageSource);
  console.log(`og -> ${outputPath.split("/").pop()}`);
}
