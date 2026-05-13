import { imageUrl, type ImageUrlOptions } from "./images";
import { getStorageBindings } from "./tako/secrets";

const DEFAULT_EXPIRES_SECONDS = 3600;
const MAX_EXPIRES_SECONDS = 7 * 24 * 60 * 60;
const SERVICE = "s3";
const SIGNING_ALGORITHM = "AWS4-HMAC-SHA256";
const UNSIGNED_PAYLOAD = "UNSIGNED-PAYLOAD";

export interface TakoStorages {}

export interface TakoStorageBinding {
  provider: "s3" | "r2";
  bucket: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  secret_access_key: string;
  force_path_style?: boolean | undefined;
  public_base_url?: string | undefined;
}

export interface CreateDownloadUrlOptions {
  /** Link lifetime. Defaults to 3600 seconds. S3-compatible services cap this at seven days. */
  expiresInSeconds?: number;
  /** Use `public_base_url` instead of signing when the storage has one configured. */
  public?: boolean;
  /** Optional S3 response content type override. */
  responseContentType?: string;
  /** Optional S3 response content disposition override. */
  responseContentDisposition?: string;
}

export interface CreateUploadUrlOptions {
  /** Link lifetime. Defaults to 3600 seconds. S3-compatible services cap this at seven days. */
  expiresInSeconds?: number;
  /** Content-Type the uploader must send with the PUT request. */
  contentType?: string;
}

export type CreateImageUrlOptions = ImageUrlOptions & {
  /** Link lifetime for private direct object URLs. Defaults to 3600 seconds. */
  expiresInSeconds?: number;
  /** Use `public_base_url` and the public image optimizer. */
  public?: boolean;
};

export interface TakoStorage {
  createDownloadUrl(key: string, options?: CreateDownloadUrlOptions): Promise<string>;
  createUploadUrl(key: string, options?: CreateUploadUrlOptions): Promise<string>;
  createImageUrl(key: string, options?: CreateImageUrlOptions): Promise<string>;
}

export type TakoStorageBag<T = TakoStorages> = Readonly<{
  [K in keyof T]: TakoStorage;
}> &
  Readonly<Record<string, TakoStorage | undefined>>;

interface StorageBagOptions {
  now?: () => Date;
}

export function loadStorages<T = TakoStorages>(): TakoStorageBag<T> {
  return createStorageBag<T>(getStorageBindings());
}

export function createStorageBag<T = TakoStorages>(
  bindings: Record<string, unknown>,
  options: StorageBagOptions = {},
): TakoStorageBag<T> {
  const storages = new Map<string, TakoStorage>();
  const now = options.now ?? (() => new Date());

  for (const [name, raw] of Object.entries(bindings)) {
    const binding = parseBinding(name, raw);
    storages.set(name, createStorage(binding, now));
  }

  return new Proxy(Object.create(null) as Record<string, TakoStorage>, {
    get(_target, prop: string | symbol): unknown {
      if (typeof prop !== "string") return undefined;
      return storages.get(prop);
    },
    ownKeys(): string[] {
      return Array.from(storages.keys());
    },
    getOwnPropertyDescriptor(_target, prop: string | symbol) {
      if (typeof prop === "string") {
        const storage = storages.get(prop);
        if (storage) {
          return { configurable: true, enumerable: false, value: storage };
        }
      }
      return undefined;
    },
    has(_target, prop: string | symbol): boolean {
      return typeof prop === "string" && storages.has(prop);
    },
  }) as TakoStorageBag<T>;
}

function createStorage(binding: TakoStorageBinding, now: () => Date): TakoStorage {
  return Object.freeze({
    createDownloadUrl(key: string, options: CreateDownloadUrlOptions = {}) {
      const publicUrl = publicObjectUrl(binding, key, options.public ?? false);
      if (publicUrl) return Promise.resolve(publicUrl);

      return presign({
        binding,
        key,
        method: "GET",
        expiresInSeconds: options.expiresInSeconds,
        query: responseOverrideQuery(options),
        headers: {},
        now,
      });
    },

    createUploadUrl(key: string, options: CreateUploadUrlOptions = {}) {
      return presign({
        binding,
        key,
        method: "PUT",
        expiresInSeconds: options.expiresInSeconds,
        query: {},
        headers: options.contentType ? { "content-type": options.contentType } : {},
        now,
      });
    },

    async createImageUrl(key: string, options: CreateImageUrlOptions = {}) {
      const { expiresInSeconds, public: usePublic, ...imageOptions } = options;
      const publicUrl = publicObjectUrl(binding, key, usePublic ?? false);
      if (publicUrl) {
        return imageUrl(publicUrl, imageOptions);
      }

      if (hasImageTransformOptions(imageOptions)) {
        throw new TypeError(
          "private storage image transforms require a public_base_url for now; use createDownloadUrl for a signed object URL",
        );
      }

      return presign({
        binding,
        key,
        method: "GET",
        expiresInSeconds,
        query: {},
        headers: {},
        now,
      });
    },
  });
}

function parseBinding(name: string, raw: unknown): TakoStorageBinding {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new TypeError(`invalid storage binding ${name}: expected object`);
  }
  const binding = raw as Partial<TakoStorageBinding>;
  for (const field of [
    "provider",
    "bucket",
    "endpoint",
    "region",
    "access_key_id",
    "secret_access_key",
  ] as const) {
    if (typeof binding[field] !== "string" || binding[field]?.trim() === "") {
      throw new TypeError(`invalid storage binding ${name}: missing ${field}`);
    }
  }
  if (binding.provider !== "s3" && binding.provider !== "r2") {
    throw new TypeError(`invalid storage binding ${name}: provider must be s3 or r2`);
  }
  return {
    provider: binding.provider,
    bucket: binding.bucket as string,
    endpoint: (binding.endpoint as string).replace(/\/+$/, ""),
    region: binding.region as string,
    access_key_id: binding.access_key_id as string,
    secret_access_key: binding.secret_access_key as string,
    force_path_style: binding.force_path_style === true,
    public_base_url: binding.public_base_url?.replace(/\/+$/, ""),
  };
}

async function presign(input: {
  binding: TakoStorageBinding;
  key: string;
  method: "GET" | "PUT";
  expiresInSeconds: number | undefined;
  query: Record<string, string>;
  headers: Record<string, string>;
  now: () => Date;
}): Promise<string> {
  const url = objectUrl(input.binding, input.key);
  for (const [key, value] of Object.entries(input.query)) {
    url.searchParams.set(key, value);
  }

  const expires = validateExpires(input.expiresInSeconds ?? DEFAULT_EXPIRES_SECONDS);
  const date = input.now();
  const amzDate = formatAmzDate(date);
  const dateStamp = amzDate.slice(0, 8);
  const scope = `${dateStamp}/${input.binding.region}/${SERVICE}/aws4_request`;
  const headers = normalizeHeaders({ host: url.host, ...input.headers });
  const signedHeaders = Object.keys(headers).sort().join(";");

  url.searchParams.set("X-Amz-Algorithm", SIGNING_ALGORITHM);
  url.searchParams.set("X-Amz-Credential", `${input.binding.access_key_id}/${scope}`);
  url.searchParams.set("X-Amz-Date", amzDate);
  url.searchParams.set("X-Amz-Expires", String(expires));
  url.searchParams.set("X-Amz-SignedHeaders", signedHeaders);

  const canonicalRequest = [
    input.method,
    canonicalUri(url),
    canonicalQuery(url.searchParams),
    canonicalHeaders(headers),
    signedHeaders,
    UNSIGNED_PAYLOAD,
  ].join("\n");

  const stringToSign = [SIGNING_ALGORITHM, amzDate, scope, await sha256Hex(canonicalRequest)].join(
    "\n",
  );
  const signingKey = await deriveSigningKey(
    input.binding.secret_access_key,
    dateStamp,
    input.binding.region,
  );
  const signature = await hmacHex(signingKey, stringToSign);
  url.searchParams.set("X-Amz-Signature", signature);
  return url.toString();
}

function objectUrl(binding: TakoStorageBinding, key: string): URL {
  const encodedKey = encodeObjectKey(key);
  const endpoint = new URL(binding.endpoint);
  if (endpoint.protocol !== "https:") {
    throw new TypeError("storage endpoint must use https");
  }

  if (binding.force_path_style) {
    endpoint.pathname = joinUrlPath(endpoint.pathname, binding.bucket, encodedKey);
    return endpoint;
  }

  endpoint.hostname = `${binding.bucket}.${endpoint.hostname}`;
  endpoint.pathname = joinUrlPath(endpoint.pathname, encodedKey);
  return endpoint;
}

function publicObjectUrl(
  binding: TakoStorageBinding,
  key: string,
  requested: boolean,
): string | null {
  if (!requested) return null;
  if (!binding.public_base_url) {
    throw new TypeError("storage does not have public_base_url configured");
  }
  const url = new URL(binding.public_base_url);
  url.pathname = joinUrlPath(url.pathname, encodeObjectKey(key));
  return url.toString();
}

function encodeObjectKey(key: string): string {
  if (key.trim() === "" || key.startsWith("/")) {
    throw new TypeError("storage key must be a non-empty relative object key");
  }
  return key.split("/").map(rfc3986Encode).join("/");
}

function joinUrlPath(...parts: string[]): string {
  const joined = parts
    .map((part) => part.replace(/^\/+|\/+$/g, ""))
    .filter(Boolean)
    .join("/");
  return `/${joined}`;
}

function responseOverrideQuery(options: CreateDownloadUrlOptions): Record<string, string> {
  const query: Record<string, string> = {};
  if (options.responseContentType) query["response-content-type"] = options.responseContentType;
  if (options.responseContentDisposition) {
    query["response-content-disposition"] = options.responseContentDisposition;
  }
  return query;
}

function hasImageTransformOptions(options: ImageUrlOptions): boolean {
  return (
    options.width !== undefined || options.quality !== undefined || options.format !== undefined
  );
}

function validateExpires(value: number): number {
  if (!Number.isInteger(value) || value < 1 || value > MAX_EXPIRES_SECONDS) {
    throw new TypeError("storage URL expiration must be an integer from 1 to 604800 seconds");
  }
  return value;
}

function normalizeHeaders(headers: Record<string, string>): Record<string, string> {
  return Object.fromEntries(
    Object.entries(headers)
      .map(([key, value]) => [key.toLowerCase(), value.trim()] as const)
      .sort(([a], [b]) => a.localeCompare(b)),
  );
}

function canonicalHeaders(headers: Record<string, string>): string {
  return Object.entries(headers)
    .map(([key, value]) => `${key}:${value.replace(/\s+/g, " ")}\n`)
    .join("");
}

function canonicalUri(url: URL): string {
  return url.pathname
    .split("/")
    .map((segment) => rfc3986Encode(decodeURIComponent(segment)))
    .join("/");
}

function canonicalQuery(params: URLSearchParams): string {
  return Array.from(params.entries())
    .sort(([aKey, aValue], [bKey, bValue]) =>
      aKey === bKey ? aValue.localeCompare(bValue) : aKey.localeCompare(bKey),
    )
    .map(([key, value]) => `${rfc3986Encode(key)}=${rfc3986Encode(value)}`)
    .join("&");
}

function rfc3986Encode(value: string): string {
  return encodeURIComponent(value).replace(
    /[!'()*]/g,
    (char) => `%${char.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

async function sha256Hex(value: string): Promise<string> {
  return bytesToHex(await subtle().digest("SHA-256", utf8(value)));
}

async function deriveSigningKey(
  secret: string,
  dateStamp: string,
  region: string,
): Promise<ArrayBuffer> {
  const kDate = await hmacBytes(utf8(`AWS4${secret}`), dateStamp);
  const kRegion = await hmacBytes(kDate, region);
  const kService = await hmacBytes(kRegion, SERVICE);
  return hmacBytes(kService, "aws4_request");
}

async function hmacBytes(key: BufferSource, value: string): Promise<ArrayBuffer> {
  const cryptoKey = await subtle().importKey("raw", key, { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
  ]);
  return subtle().sign("HMAC", cryptoKey, utf8(value));
}

async function hmacHex(key: BufferSource, value: string): Promise<string> {
  return bytesToHex(await hmacBytes(key, value));
}

function subtle(): SubtleCrypto {
  const crypto = globalThis.crypto;
  if (!crypto?.subtle) {
    throw new Error("Web Crypto is required to sign storage URLs");
  }
  return crypto.subtle;
}

function utf8(value: string): ArrayBuffer {
  return new TextEncoder().encode(value).slice().buffer as ArrayBuffer;
}

function bytesToHex(value: ArrayBuffer): string {
  return Array.from(new Uint8Array(value))
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

function formatAmzDate(date: Date): string {
  return date.toISOString().replace(/[:-]|\.\d{3}/g, "");
}
