#!/usr/bin/env bun

import { readFileSync, writeFileSync } from "node:fs";

const part = process.argv[2];
if (!part || !["patch", "minor", "major"].includes(part)) {
  console.error("usage: just sdk-rust patch|minor|major");
  process.exit(1);
}
const bump = part as "patch" | "minor" | "major";

const manifestPath = process.env.TAKO_RUST_SDK_MANIFEST ?? "sdk/rust/Cargo.toml";
const lockPath = process.env.TAKO_CARGO_LOCK ?? "Cargo.lock";

function nextVersion(version: string, part: "patch" | "minor" | "major"): string {
  const match = version.match(/^(\d+)\.(\d+)\.(\d+)$/);
  if (!match) {
    throw new Error(`Rust SDK version must be MAJOR.MINOR.PATCH, got ${version}`);
  }

  let major = Number(match[1]);
  let minor = Number(match[2]);
  let patch = Number(match[3]);

  if (part === "major") {
    major += 1;
    minor = 0;
    patch = 0;
  } else if (part === "minor") {
    minor += 1;
    patch = 0;
  } else {
    patch += 1;
  }

  return `${major}.${minor}.${patch}`;
}

function updateManifest(path: string): { oldVersion: string; newVersion: string } {
  const input = readFileSync(path, "utf8");
  let oldVersion = "";
  let newVersion = "";
  const output = input.replace(
    /(^\[package\]\n(?:[^\n]*\n)*?^version = ")([^"]+)(")/m,
    (_full, prefix: string, currentVersion: string, suffix: string) => {
      oldVersion = currentVersion;
      newVersion = nextVersion(currentVersion, bump);
      return `${prefix}${newVersion}${suffix}`;
    },
  );

  if (output === input) {
    throw new Error(`could not find [package] version in ${path}`);
  }

  writeFileSync(path, output);
  return { oldVersion, newVersion };
}

function updateLock(path: string, version: string) {
  const input = readFileSync(path, "utf8");
  const output = input.replace(
    /(\[\[package\]\]\nname = "tako"\nversion = ")([^"]+)(")/,
    `$1${version}$3`,
  );

  if (output === input) {
    throw new Error(`could not find tako package version in ${path}`);
  }

  writeFileSync(path, output);
}

try {
  const { oldVersion, newVersion } = updateManifest(manifestPath);
  updateLock(lockPath, newVersion);
  console.log(`Rust SDK version: ${oldVersion} -> ${newVersion}`);
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}
