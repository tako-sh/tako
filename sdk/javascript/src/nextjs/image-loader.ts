import { imageUrl } from "../images";

export interface NextImageLoaderParams {
  src: string;
  width: number;
  quality?: number | undefined;
}

export default function imageLoader({ src, width, quality }: NextImageLoaderParams): string {
  const options: { width: number; quality?: number } = { width };
  if (quality !== undefined) options.quality = quality;
  return imageUrl(src, options);
}
