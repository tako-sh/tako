import { isStoreDefinition, type StoreDefinition } from "./define";

/** Server-only loader registry. The proxy validates params and credentials. */
export class StoreRegistry {
  private readonly entries = new Map<string, StoreDefinition<unknown, unknown>>();

  register<Params, Data>(store: StoreDefinition<Params, Data>): void {
    if (!isStoreDefinition(store)) throw new Error("Expected a defineStore() result.");
    if (this.entries.has(store.name)) throw new Error(`duplicate store '${store.name}'`);
    this.entries.set(store.name, store as unknown as StoreDefinition<unknown, unknown>);
  }

  find(name: string): StoreDefinition<unknown, unknown> | undefined {
    return this.entries.get(name);
  }
}
