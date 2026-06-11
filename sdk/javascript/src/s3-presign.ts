import type { TakoStorageBinding } from "./storage";
import {
  DEFAULT_STORAGE_URL_EXPIRES_SECONDS,
  encodeObjectKey,
  hmacBytes,
  hmacHex,
  joinUrlPath,
  rfc3986Encode,
  sha256Hex,
  utf8,
  validateStorageUrlExpires,
} from "./storage-utils";

const SERVICE = "s3";
const SIGNING_ALGORITHM = "AWS4-HMAC-SHA256";
const UNSIGNED_PAYLOAD = "UNSIGNED-PAYLOAD";

export async function presignS3Url(input: {
  binding: TakoStorageBinding;
  key: string;
  method: "GET" | "PUT";
  expiresInSeconds: number | undefined;
  query: Record<string, string>;
  headers: Record<string, string>;
  now: () => Date;
}): Promise<string> {
  assertS3Binding(input.binding);
  const url = objectUrl(input.binding, input.key);
  for (const [key, value] of Object.entries(input.query)) {
    url.searchParams.set(key, value);
  }

  const expires = validateStorageUrlExpires(
    input.expiresInSeconds ?? DEFAULT_STORAGE_URL_EXPIRES_SECONDS,
  );
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
  assertS3Binding(binding);
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

function assertS3Binding(binding: TakoStorageBinding): asserts binding is TakoStorageBinding & {
  provider: "s3";
  bucket: string;
  endpoint: string;
  region: string;
  access_key_id: string;
  secret_access_key: string;
} {
  if (
    binding.provider !== "s3" ||
    !binding.bucket ||
    !binding.endpoint ||
    !binding.region ||
    !binding.access_key_id ||
    !binding.secret_access_key
  ) {
    throw new TypeError("storage binding is not configured for s3");
  }
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
      aKey === bKey ? compareAscii(aValue, bValue) : compareAscii(aKey, bKey),
    )
    .map(([key, value]) => `${rfc3986Encode(key)}=${rfc3986Encode(value)}`)
    .join("&");
}

function compareAscii(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
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

function formatAmzDate(date: Date): string {
  return date.toISOString().replace(/[:-]|\.\d{3}/g, "");
}
