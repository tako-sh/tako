export function normalizeCanonicalPath(pathname: string): string {
  if (pathname === "/" || pathname === "/index.html") {
    return "/";
  }

  if (pathname.endsWith("/index.html")) {
    const directoryPath = pathname.slice(0, -"/index.html".length);
    return directoryPath.endsWith("/") ? directoryPath : `${directoryPath}/`;
  }

  if (pathname.endsWith(".html")) {
    return pathname.slice(0, -".html".length) || "/";
  }

  const segment = pathname.split("/").pop() ?? "";
  if (pathname.length > 1 && !pathname.endsWith("/") && !segment.includes(".")) {
    return `${pathname}/`;
  }

  return pathname;
}

export function createCanonicalUrl(pathname: string, site: URL | undefined): URL {
  return new URL(normalizeCanonicalPath(pathname), site);
}
