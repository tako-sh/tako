import { expect } from "bun:test";

export async function expectAsyncToThrow(
  run: () => void | Promise<unknown>,
  expected?: RegExp | string,
): Promise<void> {
  try {
    await run();
  } catch (error) {
    if (expected instanceof RegExp) {
      expect(errorMessage(error)).toMatch(expected);
    } else if (typeof expected === "string") {
      expect(errorMessage(error)).toContain(expected);
    }
    return;
  }

  throw new Error("Expected function to throw");
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
