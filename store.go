package tako

import "tako.sh/internal"

// StoreDefinition loads live application data from your existing database.
// Register stores before calling ListenAndServe. ParamsSchema and Auth guard
// both reads and subscriptions; Load receives the verified subject.
type StoreDefinition = internal.StoreDefinition

// StoreLoadInput contains validated params and the authorized reader identity.
type StoreLoadInput = internal.StoreLoadInput

// StoreOfflineConfig opts a store into persisted offline reads. Private clients
// must use a separate cache scope for each user. Increment Version when the
// returned data shape changes incompatibly.
type StoreOfflineConfig = internal.StoreOfflineConfig

// Store is a registered store. Invalidate it after committing a change to its
// backing data. Invalidation signals readers to reload; it never retries writes.
type Store = internal.Store

// RegisterStore adds or replaces a store and returns its invalidation handle.
// It panics for invalid configuration, including a missing Load function.
func RegisterStore(name string, definition StoreDefinition) *Store {
	return internal.RegisterStore(name, definition)
}
