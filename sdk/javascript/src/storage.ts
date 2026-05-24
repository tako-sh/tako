import {
  imageSrcSet,
  imageUrl,
  type ImageSrcSet,
  type ImageSrcSetOptions,
  type ImageUrlOptions,
} from "./images";
import { presignS3Url } from "./s3-presign";
import {
  DEFAULT_STORAGE_URL_EXPIRES_SECONDS,
  encodeObjectKey,
  hmacHex,
  joinUrlPath,
  utf8,
  validateStorageUrlExpires,
} from "./storage-utils";
import { getStorageBindings } from "./tako/secrets";

export interface TakoStorages {}

export interface TakoStorageBinding {
  provider: "local" | "s3";
  bucket?: string | undefined;
  endpoint?: string | undefined;
  region?: string | undefined;
  access_key_id?: string | undefined;
  secret_access_key?: string | undefined;
  force_path_style?: boolean | undefined;
  public_base_url?: string | undefined;
  path?: string | undefined;
  signing_key?: string | undefined;
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

export type CreateImageSrcSetOptions = ImageSrcSetOptions & {
  /** Use `public_base_url` and the public image optimizer. Required until private image transforms are supported. */
  public?: boolean;
};

export interface TakoStorage {
  createDownloadUrl(key: string, options?: CreateDownloadUrlOptions): Promise<string>;
  createUploadUrl(key: string, options?: CreateUploadUrlOptions): Promise<string>;
  createImageUrl(key: string, options?: CreateImageUrlOptions): Promise<string>;
  createImageSrcSet(key: string, options: CreateImageSrcSetOptions): Promise<ImageSrcSet>;
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
    storages.set(name, createStorage(name, binding, now));
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

function createStorage(name: string, binding: TakoStorageBinding, now: () => Date): TakoStorage {
  return Object.freeze({
    createDownloadUrl(key: string, options: CreateDownloadUrlOptions = {}) {
      if (binding.provider === "local") {
        return localStorageUrl(name, binding, "GET", key, options.expiresInSeconds, now);
      }
      const publicUrl = publicObjectUrl(binding, key, options.public ?? false);
      if (publicUrl) return Promise.resolve(publicUrl);

      return presignS3Url({
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
      if (binding.provider === "local") {
        return localStorageUrl(name, binding, "PUT", key, options.expiresInSeconds, now);
      }
      return presignS3Url({
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
      if (binding.provider === "local") {
        if (hasImageTransformOptions(imageOptions)) {
          throw new TypeError("local storage image transforms require public storage for now");
        }
        return localStorageUrl(name, binding, "GET", key, expiresInSeconds, now);
      }
      const publicUrl = publicObjectUrl(binding, key, usePublic ?? false);
      if (publicUrl) {
        return imageUrl(publicUrl, imageOptions);
      }

      if (hasImageTransformOptions(imageOptions)) {
        throw new TypeError(
          "private storage image transforms require a public_base_url for now; use createDownloadUrl for a signed object URL",
        );
      }

      return presignS3Url({
        binding,
        key,
        method: "GET",
        expiresInSeconds,
        query: {},
        headers: {},
        now,
      });
    },

    async createImageSrcSet(key: string, options: CreateImageSrcSetOptions) {
      const { public: usePublic, ...imageOptions } = options;
      const publicUrl = publicObjectUrl(binding, key, usePublic ?? false);
      if (publicUrl) {
        return imageSrcSet(publicUrl, imageOptions);
      }

      throw new TypeError(
        "private storage image srcsets require a public_base_url for now; use createDownloadUrl for a signed object URL",
      );
    },
  });
}

function parseBinding(name: string, raw: unknown): TakoStorageBinding {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new TypeError(`invalid storage binding ${name}: expected object`);
  }
  const binding = raw as Partial<TakoStorageBinding>;
  if (binding.provider !== "local" && binding.provider !== "s3") {
    throw new TypeError(`invalid storage binding ${name}: provider must be local or s3`);
  }

  if (binding.provider === "local") {
    for (const field of ["path", "signing_key"] as const) {
      if (typeof binding[field] !== "string" || binding[field]?.trim() === "") {
        throw new TypeError(`invalid storage binding ${name}: missing ${field}`);
      }
    }
    return {
      provider: "local",
      path: binding.path as string,
      signing_key: binding.signing_key as string,
    };
  }

  for (const field of [
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
  return {
    provider: "s3",
    bucket: binding.bucket as string,
    endpoint: (binding.endpoint as string).replace(/\/+$/, ""),
    region: binding.region as string,
    access_key_id: binding.access_key_id as string,
    secret_access_key: binding.secret_access_key as string,
    force_path_style: binding.force_path_style === true,
    public_base_url: binding.public_base_url?.replace(/\/+$/, ""),
  };
}

function publicObjectUrl(
  binding: TakoStorageBinding,
  key: string,
  requested: boolean,
): string | null {
  if (binding.provider !== "s3") return null;
  if (!requested) return null;
  if (!binding.public_base_url) {
    throw new TypeError("storage does not have public_base_url configured");
  }
  const url = new URL(binding.public_base_url);
  url.pathname = joinUrlPath(url.pathname, encodeObjectKey(key));
  return url.toString();
}

function localStorageUrl(
  name: string,
  binding: TakoStorageBinding,
  method: "GET" | "PUT",
  key: string,
  expiresInSeconds: number | undefined,
  now: () => Date,
): Promise<string> {
  if (binding.provider !== "local" || !binding.path || !binding.signing_key) {
    throw new TypeError("storage binding is not configured for local storage");
  }
  const expires =
    Math.floor(now().getTime() / 1000) +
    validateStorageUrlExpires(expiresInSeconds ?? DEFAULT_STORAGE_URL_EXPIRES_SECONDS);
  const encodedKey = encodeObjectKey(key);
  const signingKey = binding.signing_key;
  const payload = `${method}\n${name}\n${encodedKey}\n${expires}`;
  return hmacHex(utf8(signingKey), payload).then((token) => {
    return `/_tako/storages/${encodeURIComponent(name)}/${encodedKey}?expires=${expires}&token=${token}`;
  });
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
