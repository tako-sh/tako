import { imageUrl, type ImageUrlOptions } from "tako.sh";

export function demoImageUrl(source: string, options: ImageUrlOptions): string {
  return imageUrl(source, options);
}
