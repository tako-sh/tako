export function normalizeCanonicalPath(pathname: string): string {
  if (pathname === "/" || pathname === "/index.html") {
    return "/";
  }

  if (pathname.endsWith("/index.html")) {
    const directoryPath = pathname.slice(0, -"/index.html".length);
    return directoryPath.replace(/\/$/, "") || "/";
  }

  if (pathname.endsWith(".html")) {
    return pathname.slice(0, -".html".length) || "/";
  }

  return pathname.replace(/\/$/, "") || "/";
}

export function createCanonicalUrl(pathname: string, site: URL | undefined): URL {
  return new URL(normalizeCanonicalPath(pathname), site);
}
