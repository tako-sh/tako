import { join } from "node:path";

export const APP_ROOT_ENV = "TAKO_APP_ROOT";
export const DEFAULT_APP_ROOT = "src";

export function resolveAppRootDir(appDir: string, appRoot?: string): string {
  const root = (appRoot ?? process.env[APP_ROOT_ENV] ?? DEFAULT_APP_ROOT).trim();
  if (!root || root === ".") return appDir;
  return join(appDir, root);
}
