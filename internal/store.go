package internal

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"regexp"
	"sync"
)

const storeChannelPrefix = "tako.store:"
const internalStoreLoadPath = "/stores/load"
const maxStoreParamsBytes = 16 * 1024

var storeNamePattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,255}$`)

// StoreOfflineConfig is the server's opt-in policy for cached offline reads.
type StoreOfflineConfig struct {
	// MaxAgeMs is the maximum age of a persisted result, in milliseconds.
	MaxAgeMs int64 `json:"maxAgeMs"`
	// Version identifies the cached result shape. An empty version becomes "1".
	Version string `json:"version"`
}

// StoreLoadInput comes from the authorized native proxy, never from a public
// request. Params has already been validated against ParamsSchema.
type StoreLoadInput struct {
	Store   string          `json:"store"`
	Params  json.RawMessage `json:"params"`
	Subject string          `json:"subject,omitempty"`
}

// StoreDefinition configures a live view of application-owned backend data.
type StoreDefinition struct {
	ParamsSchema json.RawMessage
	Auth         *ChannelAuthScheme
	Verify       func(VerifyInput) ChannelAuthDecision
	Load         func(context.Context, StoreLoadInput) (any, error)
	Offline      *StoreOfflineConfig
	// KeepaliveIntervalMs controls subscription keepalive comments.
	KeepaliveIntervalMs int64
	// MaxConnectionLifetimeMs bounds the time until a subscription reauthorizes.
	MaxConnectionLifetimeMs int64
}

// Store is an invalidation handle for a registered store.
type Store struct {
	name      string
	lifecycle ChannelLifecycleConfig
}

// Name is the public name passed to RegisterStore.
func (s *Store) Name() string { return s.name }

// Invalidate notifies readers of these params to reload after a committed write.
// Nil params means an empty object, not all parameter scopes. It returns an error
// if publication fails; callers requiring atomic write-and-notify should use an
// application outbox. No database operation is retried.
func (s *Store) Invalidate(ctx context.Context, params any) error {
	if s == nil {
		return errors.New("tako: store is nil")
	}
	if params == nil {
		params = struct{}{}
	}
	scope, err := json.Marshal(params)
	if err != nil || !isJSONObject(scope) {
		return errors.New("tako: store params must be a JSON object")
	}
	if len(scope) > maxStoreParamsBytes {
		return errors.New("tako: store params must be at most 16 KiB")
	}
	return publishStoreInvalidation(ctx, s.name, scope, s.lifecycle)
}

var stores = struct {
	sync.RWMutex
	definitions map[string]StoreDefinition
}{definitions: make(map[string]StoreDefinition)}

// RegisterStore adds or replaces a live store and its read-only subscription.
// Configuration errors panic at startup rather than failing after deployment.
func RegisterStore(name string, definition StoreDefinition) *Store {
	if !storeNamePattern.MatchString(name) {
		panic("tako: store name must be 1–256 ASCII letters, digits, dots, underscores, or hyphens, starting with a letter or digit")
	}
	if definition.Load == nil {
		panic("tako: store Load is required")
	}
	if definition.KeepaliveIntervalMs < 0 || definition.KeepaliveIntervalMs > 9_007_199_254_740_991 ||
		definition.MaxConnectionLifetimeMs < 0 || definition.MaxConnectionLifetimeMs > 9_007_199_254_740_991 {
		panic("tako: store lifecycle durations must be nonnegative safe integers")
	}
	if definition.Offline != nil {
		offline := *definition.Offline
		if offline.MaxAgeMs <= 0 || offline.MaxAgeMs > 9_007_199_254_740_991 {
			panic("tako: store offline MaxAgeMs must be a positive safe integer")
		}
		if offline.Version == "" {
			offline.Version = "1"
		}
		if len(offline.Version) > 128 {
			panic("tako: store offline Version must be at most 128 bytes")
		}
		definition.Offline = &offline
	}
	if definition.ParamsSchema != nil {
		if !isJSONObject(definition.ParamsSchema) {
			panic("tako: store ParamsSchema must be a JSON object")
		}
		definition.ParamsSchema = append(json.RawMessage(nil), definition.ParamsSchema...)
	}
	if definition.Auth != nil {
		auth := *definition.Auth
		definition.Auth = &auth
	}
	lifecycle := ChannelLifecycleConfig{
		KeepaliveIntervalMs:     definition.KeepaliveIntervalMs,
		MaxConnectionLifetimeMs: definition.MaxConnectionLifetimeMs,
	}
	verify := definition.Verify
	if verify != nil {
		verify = func(input VerifyInput) ChannelAuthDecision {
			if input.Operation != ChannelOperationSubscribe {
				return RejectChannel()
			}
			input.Channel = name
			return definition.Verify(input)
		}
	}
	Channels.Register(storeChannelPrefix+name, ChannelDefinition{
		ParamsSchema:           definition.ParamsSchema,
		Auth:                   definition.Auth,
		Verify:                 verify,
		ChannelLifecycleConfig: lifecycle,
	})
	stores.Lock()
	stores.definitions[name] = definition
	stores.Unlock()
	return &Store{name: name, lifecycle: lifecycle.withDefaults()}
}

func isJSONObject(value []byte) bool {
	var object map[string]json.RawMessage
	return json.Unmarshal(value, &object) == nil && object != nil
}

func writeStoreJSON(w http.ResponseWriter, status int, value any, token string) {
	body, err := json.Marshal(value)
	if err != nil || len(body) > maxInternalBodyBytes {
		status = http.StatusInternalServerError
		body = []byte(`{"error":"Store load failed"}`)
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	if token != "" {
		w.Header().Set(internalTokenHeader, token)
	}
	w.WriteHeader(status)
	_, _ = w.Write(body)
}

func (h *EndpointHandler) handleStoreLoad(w http.ResponseWriter, r *http.Request) {
	if h.internalToken == "" {
		writeStoreJSON(w, http.StatusForbidden, map[string]string{"error": "Forbidden"}, "")
		return
	}
	if r.Method != http.MethodPost {
		w.Header().Set("Allow", http.MethodPost)
		writeStoreJSON(w, http.StatusMethodNotAllowed, map[string]string{"error": "Method not allowed"}, h.internalToken)
		return
	}
	decoder := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxInternalBodyBytes))
	decoder.DisallowUnknownFields()
	var input StoreLoadInput
	if err := decoder.Decode(&input); err != nil || input.Store == "" || !isJSONObject(input.Params) {
		writeStoreJSON(w, http.StatusBadRequest, map[string]string{"error": "Invalid request"}, h.internalToken)
		return
	}
	if err := decoder.Decode(new(any)); !errors.Is(err, io.EOF) {
		writeStoreJSON(w, http.StatusBadRequest, map[string]string{"error": "Invalid request"}, h.internalToken)
		return
	}
	stores.RLock()
	definition, found := stores.definitions[input.Store]
	stores.RUnlock()
	if !found {
		writeStoreJSON(w, http.StatusNotFound, map[string]string{"error": "Store not defined"}, h.internalToken)
		return
	}
	// Neither registry lock is held while application code runs.
	data, err := definition.Load(r.Context(), input)
	if err != nil {
		writeStoreJSON(w, http.StatusInternalServerError, map[string]string{"error": "Store load failed"}, h.internalToken)
		return
	}
	writeStoreJSON(w, http.StatusOK, struct {
		Data    any                 `json:"data"`
		Offline *StoreOfflineConfig `json:"offline,omitempty"`
	}{Data: data, Offline: definition.Offline}, h.internalToken)
}
