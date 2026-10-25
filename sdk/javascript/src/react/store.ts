import { useMemo, useSyncExternalStore } from "react";
import type { TakoStores } from "tako.sh";
import { Store, storeIdentity, type StoreOptions, type StoreSnapshot } from "../stores/client";

export type UseStoreOptions = StoreOptions;

export interface StoreConnection<T> extends StoreSnapshot<T> {
  /** Refetch now; useful after an application-specific write. */
  refresh(): Promise<void>;
}

type RegisteredName = Extract<keyof TakoStores, string>;
type Entry<Name extends RegisteredName> = TakoStores[Name] extends {
  params: infer Params extends Record<string, unknown>;
  data: infer Data;
}
  ? { params: Params; data: Data }
  : { params: Record<string, never>; data: unknown };
type RegisteredOptions<Name extends RegisteredName> = Omit<UseStoreOptions, "params"> & {
  params: Entry<Name>["params"];
};

export function useStore<Name extends RegisteredName>(
  name: Name,
  ...options: Record<string, never> extends Entry<Name>["params"]
    ? [options?: Omit<RegisteredOptions<Name>, "params"> & { params?: Entry<Name>["params"] }]
    : [options: RegisteredOptions<Name>]
): StoreConnection<Entry<Name>["data"]>;
/** Typed fallback for projects without generated Store declarations. */
export function useStore<T = unknown>(
  name: RegisteredName extends never ? string : never,
  options?: UseStoreOptions,
): StoreConnection<T>;
/** Dynamic or unregistered names remain usable without bypassing known params. */
export function useStore<Name extends string>(
  name: Name extends RegisteredName ? never : Name,
  options?: UseStoreOptions,
): StoreConnection<unknown>;

/** Subscribe to backend data with reconnect catch-up and optional offline reads. */
export function useStore<T = unknown>(
  name: string,
  options: UseStoreOptions = {},
): StoreConnection<T> {
  const identity = storeIdentity(name, options);
  const store = useMemo(
    () => new Store<T>(name, options),
    // Values are captured by the full identity; dependencies with identity matter separately.
    [identity, options.fetch, options.cache],
  );
  const snapshot = useSyncExternalStore(
    store.subscribe,
    store.getSnapshot,
    store.getServerSnapshot,
  );
  return useMemo(() => ({ ...snapshot, refresh: store.refresh }), [snapshot, store]);
}
