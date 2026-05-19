export interface ParsedHost {
  baseSlug?: string;
  routeStyle: "path" | "subdomain";
  rootHost: string;
  rootOrigin: string;
}

const DEFAULT_ROOT = "demo.tako.sh";
const WILDCARD_ROOTS = ["demo.tako.sh", "demo.test"];
const BASE_SUBDOMAIN = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;

export function parseHost(hostHeader: string): ParsedHost {
  const { host, port } = splitHostPort(hostHeader);
  const currentHost = host || DEFAULT_ROOT;
  const wildcardRoot = WILDCARD_ROOTS.find(
    (root) => currentHost === root || currentHost.endsWith(`.${root}`),
  );
  const rootHost = wildcardRoot ?? currentHost;
  const rootOrigin = `//${rootHost}${port ? `:${port}` : ""}`;

  if (!wildcardRoot) {
    return { routeStyle: "path", rootHost, rootOrigin };
  }

  const maybeBase =
    currentHost === wildcardRoot
      ? null
      : currentHost.slice(0, currentHost.length - wildcardRoot.length - 1);
  const baseSlug = maybeBase && BASE_SUBDOMAIN.test(maybeBase) ? maybeBase : undefined;

  return {
    ...(baseSlug ? { baseSlug } : {}),
    routeStyle: "subdomain",
    rootHost,
    rootOrigin,
  };
}

export function baseHref(parsedHost: ParsedHost, slug: string): string {
  if (parsedHost.routeStyle === "subdomain") {
    return (
      parsedHost.rootOrigin.replace(
        `//${parsedHost.rootHost}`,
        `//${slug}.${parsedHost.rootHost}`,
      ) + "/"
    );
  }

  return `/bases/${slug}`;
}

function splitHostPort(hostHeader: string): { host: string; port: string | null } {
  if (!hostHeader) {
    return { host: "", port: null };
  }

  const value = hostHeader.trim().toLowerCase();
  if (value.startsWith("[")) {
    const close = value.indexOf("]");
    if (close === -1) {
      return { host: value, port: null };
    }
    return {
      host: value.slice(0, close + 1),
      port: value[close + 1] === ":" ? value.slice(close + 2) : null,
    };
  }

  const colon = value.lastIndexOf(":");
  if (colon === -1 || value.indexOf(":") !== colon) {
    return { host: value, port: null };
  }

  return { host: value.slice(0, colon), port: value.slice(colon + 1) };
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
