import { imageUrl, type ImageUrlOptions } from "tako.sh";

export function demoImageUrl(source: string, options: ImageUrlOptions): string {
  if (import.meta.env.DEV) return source;
  return imageUrl(source, options);
}
