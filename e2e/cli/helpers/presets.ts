import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

/** Terminal tests exercise the wizard against this checkout, without live GitHub requests. */
export async function seedPresets(takoHome: string): Promise<void> {
  const repo = resolve(import.meta.dirname, "../../..");
  const cache = join(takoHome, "presets", "tako-sh__tako");
  const sha = "c".repeat(40);
  await mkdir(join(cache, sha, "presets"), { recursive: true });
  for (const name of ["javascript", "go"]) {
    await writeFile(
      join(cache, sha, "presets", `${name}.toml`),
      await readFile(join(repo, "presets", `${name}.toml`)),
    );
  }
  await writeFile(
    join(cache, "_meta.json"),
    JSON.stringify({ branches: { master: { sha, last_checked: Math.floor(Date.now() / 1000) } } }),
  );
}
