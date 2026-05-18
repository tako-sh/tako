export interface ParsedHost {
  rootHost: string;
  rootOrigin: string;
}

const DEFAULT_ROOT = "demo.tako.sh";

export function parseHost(hostHeader: string): ParsedHost {
  const [hostPart, port] = hostHeader.split(":");
  const rootHost = hostPart || DEFAULT_ROOT;
  const rootOrigin = `//${port ? `${rootHost}:${port}` : rootHost}`;
  return { rootHost, rootOrigin };
}

export function sanitizeBaseSlug(raw: string): string {
  const cleaned = raw
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, "")
    .slice(0, 48);
  return cleaned.length > 0 ? cleaned : "base";
}

export function prettifyBaseSlug(slug: string): string {
  return slug
    .split("-")
    .filter((part) => part.length > 0)
    .map((part) => `${part.charAt(0).toUpperCase()}${part.slice(1)}`)
    .join(" ");
}
