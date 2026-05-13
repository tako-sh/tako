export interface ParsedHost {
  tenantSlug: string | null;
  rootHost: string;
  rootOrigin: string;
  canonicalOrigin: string;
}

const DEFAULT_ROOT = "demo.tako.sh";

export function parseHost(hostHeader: string): ParsedHost {
  const [hostPart, port] = hostHeader.split(":");
  const host = hostPart ?? "";
  const labels = host.split(".");
  const demoIndex = labels.indexOf("demo");
  if (demoIndex === -1) {
    const rootHost = host || DEFAULT_ROOT;
    const rootOrigin = `//${port ? `${rootHost}:${port}` : rootHost}`;
    return {
      tenantSlug: null,
      rootHost,
      rootOrigin,
      canonicalOrigin: rootOrigin,
    };
  }
  const rootHost = labels.slice(demoIndex).join(".");
  const tenantSlug = demoIndex === 1 ? (labels[0] ?? null) : null;
  const rootOrigin = `//${port ? `${rootHost}:${port}` : rootHost}`;
  const canonicalHost = tenantSlug ? `${tenantSlug}.${rootHost}` : rootHost;
  const canonicalOrigin = `//${port ? `${canonicalHost}:${port}` : canonicalHost}`;
  return {
    tenantSlug,
    rootHost,
    rootOrigin,
    canonicalOrigin,
  };
}

export function parseTenant(hostHeader: string): string | null {
  return parseHost(hostHeader).tenantSlug;
}

export function sanitizeTenantSlug(raw: string): string {
  const cleaned = raw
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, "")
    .slice(0, 48);
  return cleaned.length > 0 ? cleaned : "base";
}

export function prettifyTenantSlug(slug: string): string {
  return slug
    .split("-")
    .filter((part) => part.length > 0)
    .map((part) => `${part.charAt(0).toUpperCase()}${part.slice(1)}`)
    .join(" ");
}
