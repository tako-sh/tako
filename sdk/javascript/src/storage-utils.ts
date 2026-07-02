export const DEFAULT_STORAGE_URL_EXPIRES_SECONDS = 3600;
export const MAX_STORAGE_URL_EXPIRES_SECONDS = 7 * 24 * 60 * 60;

export function encodeObjectKey(key: string): string {
  if (key.trim() === "" || key.startsWith("/")) {
    throw new TypeError("storage key must be a non-empty relative object key");
  }
  const segments = key.split("/");
  // `.`/`..` segments survive percent-encoding and get normalized away by
  // URL parsers, letting a key escape the configured base prefix.
  if (segments.some((segment) => segment === "." || segment === "..")) {
    throw new TypeError("storage key must not contain '.' or '..' path segments");
  }
  return segments.map(rfc3986Encode).join("/");
}

export function joinUrlPath(...parts: string[]): string {
  const joined = parts
    .map((part) => part.replace(/^\/+|\/+$/g, ""))
    .filter(Boolean)
    .join("/");
  return `/${joined}`;
}

export function validateStorageUrlExpires(value: number): number {
  if (!Number.isInteger(value) || value < 1 || value > MAX_STORAGE_URL_EXPIRES_SECONDS) {
    throw new TypeError("storage URL expiration must be an integer from 1 to 604800 seconds");
  }
  return value;
}

export function rfc3986Encode(value: string): string {
  return encodeURIComponent(value).replace(
    /[!'()*]/g,
    (char) => `%${char.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

export async function sha256Hex(value: string): Promise<string> {
  return bytesToHex(await subtle().digest("SHA-256", utf8(value)));
}

export async function hmacBytes(key: BufferSource, value: string): Promise<ArrayBuffer> {
  const cryptoKey = await subtle().importKey("raw", key, { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
  ]);
  return subtle().sign("HMAC", cryptoKey, utf8(value));
}

export async function hmacHex(key: BufferSource, value: string): Promise<string> {
  return bytesToHex(await hmacBytes(key, value));
}

export function utf8(value: string): ArrayBuffer {
  return new TextEncoder().encode(value).slice().buffer as ArrayBuffer;
}

export function bytesToHex(value: ArrayBuffer): string {
  return Array.from(new Uint8Array(value))
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function subtle(): SubtleCrypto {
  const crypto = globalThis.crypto;
  if (!crypto?.subtle) {
    throw new Error("Web Crypto is required to sign storage URLs");
  }
  return crypto.subtle;
}
