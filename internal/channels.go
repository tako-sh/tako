package internal

import (
	"encoding/json"
	"sort"
	"strconv"
	"strings"
	"sync"
)

const (
	// DefaultChannelRetentionMs is the default replay window for channel events.
	DefaultChannelRetentionMs int64 = 10 * 60 * 1000
	// DefaultChannelInactivityTtlMs disables idle timeout by default.
	DefaultChannelInactivityTtlMs int64 = 0
	// DefaultChannelKeepaliveIntervalMs is the default SSE keepalive interval.
	DefaultChannelKeepaliveIntervalMs int64 = 25 * 1000
	// DefaultChannelMaxConnectionLifetimeMs is the default maximum connection age.
	DefaultChannelMaxConnectionLifetimeMs int64 = 2 * 60 * 60 * 1000
)

// ChannelTransport identifies the live transport used by a channel.
type ChannelTransport string

const (
	// ChannelTransportWS enables WebSocket channels.
	ChannelTransportWS ChannelTransport = "ws"
)

// ChannelOperation is the operation being authorized for a channel request.
type ChannelOperation string

const (
	// ChannelOperationSubscribe authorizes a channel subscription.
	ChannelOperationSubscribe ChannelOperation = "subscribe"
	// ChannelOperationPublish authorizes a server-side publish.
	ChannelOperationPublish ChannelOperation = "publish"
	// ChannelOperationConnect authorizes a WebSocket connection.
	ChannelOperationConnect ChannelOperation = "connect"
)

// ChannelLifecycleConfig controls replay, idle, keepalive, and max lifetime
// behavior for channel subscriptions.
type ChannelLifecycleConfig struct {
	// Ephemeral delivers live events without persistence or replay.
	Ephemeral bool `json:"ephemeral,omitempty"`
	// ReplayWindowMs is how long events remain available for resume/replay.
	ReplayWindowMs int64 `json:"replayWindowMs,omitempty"`
	// InactivityTtlMs closes idle subscriptions after the given duration.
	InactivityTtlMs int64 `json:"inactivityTtlMs"`
	// KeepaliveIntervalMs controls SSE keepalive comments.
	KeepaliveIntervalMs int64 `json:"keepaliveIntervalMs,omitempty"`
	// MaxConnectionLifetimeMs closes long-lived subscriptions after the given duration.
	MaxConnectionLifetimeMs int64 `json:"maxConnectionLifetimeMs,omitempty"`
}

func (c ChannelLifecycleConfig) withDefaults() ChannelLifecycleConfig {
	if c.ReplayWindowMs == 0 && !c.Ephemeral {
		c.ReplayWindowMs = DefaultChannelRetentionMs
	}
	if c.KeepaliveIntervalMs == 0 {
		c.KeepaliveIntervalMs = DefaultChannelKeepaliveIntervalMs
	}
	if c.MaxConnectionLifetimeMs == 0 {
		c.MaxConnectionLifetimeMs = DefaultChannelMaxConnectionLifetimeMs
	}
	return c
}

// ChannelHeaderValue is a parsed authorization header value.
type ChannelHeaderValue struct {
	// Scheme is the header auth scheme, for example "Bearer".
	Scheme string `json:"scheme,omitempty"`
	// Value is the credential value after the optional scheme.
	Value string `json:"value"`
}

// ParseChannelHeaderValue splits an authorization header into scheme and value.
func ParseChannelHeaderValue(raw string) ChannelHeaderValue {
	if idx := strings.IndexByte(raw, ' '); idx >= 0 {
		return ChannelHeaderValue{Scheme: raw[:idx], Value: raw[idx+1:]}
	}
	return ChannelHeaderValue{Value: raw}
}

// ChannelAuthScheme describes where channel credentials are read from.
type ChannelAuthScheme struct {
	// HeaderName is the request header name used for authorization.
	HeaderName string `json:"headerName,omitempty"`
	// CookieName is the cookie name used for authorization.
	CookieName string `json:"cookieName,omitempty"`
}

// VerifyInput is passed to a channel authorization function.
type VerifyInput struct {
	// Channel is the channel name being authorized.
	Channel string `json:"channel"`
	// Operation is the channel action being authorized.
	Operation ChannelOperation `json:"operation"`
	// Params is the validated channel params payload.
	Params json.RawMessage `json:"params"`
	// Header is the parsed authorization header, when configured and present.
	Header *ChannelHeaderValue `json:"header,omitempty"`
	// Cookie is the authorization cookie value, when configured and present.
	Cookie *string `json:"cookie,omitempty"`
}

// ChannelGrant describes the authorized subject and optional lifecycle
// overrides for an accepted channel request. Zero lifecycle fields inherit
// the corresponding definition setting. Ephemeral is always inherited from the
// definition; grants cannot change the delivery mode.
type ChannelGrant struct {
	// Subject is the application-defined authorized identity.
	Subject string `json:"subject,omitempty"`
	ChannelLifecycleConfig
}

// ChannelAuthDecision is the result returned by a channel authorization
// function.
type ChannelAuthDecision struct {
	// OK is true when the request is accepted.
	OK bool `json:"ok"`
	ChannelGrant
}

// AllowChannel accepts a channel authorization request.
func AllowChannel(grant ChannelGrant) ChannelAuthDecision {
	return ChannelAuthDecision{
		OK:           true,
		ChannelGrant: grant,
	}
}

// RejectChannel denies a channel authorization request.
func RejectChannel() ChannelAuthDecision {
	return ChannelAuthDecision{OK: false}
}

// ChannelDefinition describes a channel registered with a [ChannelRegistry].
type ChannelDefinition struct {
	ChannelLifecycleConfig
	// ParamsSchema is a JSON Schema for the query params that bind a channel.
	// Validated params select the channel stream, so it must be one object
	// schema with flat string, integer, number, or boolean properties under
	// its top-level "properties" (no $ref; a top-level allOf, anyOf, oneOf,
	// not, if/then/else, or dependentSchemas may only add constraints such as
	// "required" over those properties; Register panics otherwise). tako-server rejects a param the
	// schema does not list under "properties" unless it sets
	// "additionalProperties": true; nil means the channel takes no params. "from" and "last_message_id"
	// are reserved query names, so [ChannelRegistry.Register] panics if a
	// property uses one. Verify must still check that the caller may use the
	// requested params.
	ParamsSchema json.RawMessage
	// Auth configures where credentials are read from. Nil means public.
	Auth *ChannelAuthScheme
	// Verify authorizes authenticated channel operations.
	Verify func(VerifyInput) ChannelAuthDecision
	// Transport is the live channel transport.
	Transport ChannelTransport
}

// ChannelAuthorizeInput is the internal authorization request shape used by
// Tako channel endpoints.
type ChannelAuthorizeInput struct {
	// Channel is the channel name being authorized.
	Channel string `json:"channel"`
	// Operation is the channel action being authorized.
	Operation ChannelOperation `json:"operation"`
	// Params is the validated channel params payload.
	Params json.RawMessage `json:"params"`
	// Header is the parsed authorization header, when present.
	Header *ChannelHeaderValue `json:"header,omitempty"`
	// Cookie is the authorization cookie value, when present.
	Cookie *string `json:"cookie,omitempty"`
}

// ChannelAuthorizeResponse is the internal authorization response shape used by
// Tako channel endpoints.
type ChannelAuthorizeResponse struct {
	// OK is true when the request is accepted.
	OK bool `json:"ok"`
	// Transport is the authorized live transport.
	Transport ChannelTransport `json:"transport,omitempty"`
	ChannelGrant
}

// ChannelAuthMetadata is the serialized auth metadata exposed by discovery.
type ChannelAuthMetadata struct {
	// Public is true when the channel has no auth scheme.
	Public bool
	// Scheme is the configured auth scheme for private channels.
	Scheme ChannelAuthScheme
}

func (m ChannelAuthMetadata) MarshalJSON() ([]byte, error) {
	if m.Public {
		return []byte("false"), nil
	}
	return json.Marshal(m.Scheme)
}

// ChannelDefinitionMeta is the public metadata shape returned by the channel
// registry endpoint.
type ChannelDefinitionMeta struct {
	ChannelLifecycleConfig
	// Channel is the registered channel name.
	Channel string `json:"channel"`
	// ParamsSchema is the JSON Schema clients use to bind channel params.
	ParamsSchema json.RawMessage `json:"paramsSchema"`
	// Auth describes whether and how the channel is authorized.
	Auth ChannelAuthMetadata `json:"auth"`
	// Transport is the live channel transport.
	Transport ChannelTransport `json:"transport,omitempty"`
}

// ChannelRegistry stores channel definitions for discovery and authorization.
type ChannelRegistry struct {
	mu          sync.RWMutex
	definitions map[string]ChannelDefinition
}

// NewChannelRegistry creates an empty channel registry.
func NewChannelRegistry() *ChannelRegistry {
	return &ChannelRegistry{definitions: map[string]ChannelDefinition{}}
}

// Channels is the process-local channel registry used by Tako endpoints.
var Channels = NewChannelRegistry()

// ReservedChannelParamNames are the query names Tako reserves on channel
// routes: "from" (where a reader starts) and "last_message_id" (the
// WebSocket replay cursor). Channel and presence params cannot use them.
var ReservedChannelParamNames = []string{"from", "last_message_id"}

// Register adds or replaces a channel definition. It panics if the
// definition is invalid, including a params schema property named "from" or
// "last_message_id".
func (r *ChannelRegistry) Register(name string, definition ChannelDefinition) {
	if definition.Ephemeral && definition.ReplayWindowMs != 0 {
		panic("ephemeral channels cannot set ReplayWindowMs")
	}
	// Store reads carry params as JSON; channel and presence params are query values.
	if !strings.HasPrefix(name, storeChannelPrefix) {
		if problem := nonFlatParamsSchema(definition.ParamsSchema); problem != "" {
			panic("tako: channel " + strconv.Quote(name) + " params schema " + problem + "; declare every param under the top-level \"properties\" of one object schema")
		}
		if reserved := reservedParamName(definition.ParamsSchema); reserved != "" {
			panic("tako: channel " + strconv.Quote(name) + " declares param " + strconv.Quote(reserved) + ", a name Tako reserves on channel routes; rename the param")
		}
	}
	if definition.ParamsSchema == nil {
		// Params select channel and presence streams, so tako-server rejects
		// params a schema does not declare; without a schema there are none.
		// Store params are JSON validated as given.
		if strings.HasPrefix(name, storeChannelPrefix) {
			definition.ParamsSchema = json.RawMessage(`{"type":"object"}`)
		} else {
			definition.ParamsSchema = json.RawMessage(`{"type":"object","additionalProperties":false}`)
		}
	}
	if definition.Auth != nil && definition.Auth.HeaderName == "" && definition.Auth.CookieName == "" {
		definition.Auth.HeaderName = "authorization"
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	r.definitions[name] = definition
}

// nonFlatSchemaKeywords place properties outside a schema's top-level
// "properties", where channel params are declared and coerced.
var nonFlatSchemaKeywords = []string{"$ref", "$dynamicRef", "patternProperties"}

// Combinators whose subschemas apply to the whole params object. They may add
// constraints such as "required" over the declared params, but a param
// declared inside one would be neither coerced nor allowed.
var (
	arrayCombinators  = []string{"allOf", "anyOf", "oneOf"}
	schemaCombinators = []string{"not", "if", "then", "else"}
	mapCombinators    = []string{"dependentSchemas"}
)

// declaringKeywords declare params when a combinator subschema uses them.
var declaringKeywords = []string{"properties", "patternProperties", "additionalProperties", "$ref", "$dynamicRef"}

type combinatorSubschema struct {
	combinator string
	schema     json.RawMessage
}

// combinatorSubschemas lists the subschemas a schema's combinators apply to
// the same instance, each with the combinator that holds it.
func combinatorSubschemas(schema map[string]json.RawMessage) []combinatorSubschema {
	var subschemas []combinatorSubschema
	for _, keyword := range arrayCombinators {
		var items []json.RawMessage
		if raw, ok := schema[keyword]; ok && json.Unmarshal(raw, &items) == nil {
			for _, item := range items {
				subschemas = append(subschemas, combinatorSubschema{keyword, item})
			}
		}
	}
	for _, keyword := range schemaCombinators {
		if raw, ok := schema[keyword]; ok {
			subschemas = append(subschemas, combinatorSubschema{keyword, raw})
		}
	}
	for _, keyword := range mapCombinators {
		var entries map[string]json.RawMessage
		if raw, ok := schema[keyword]; ok && json.Unmarshal(raw, &entries) == nil {
			names := make([]string, 0, len(entries))
			for name := range entries {
				names = append(names, name)
			}
			sort.Strings(names)
			for _, name := range names {
				subschemas = append(subschemas, combinatorSubschema{keyword, entries[name]})
			}
		}
	}
	return subschemas
}

// declaringKeyword returns the keyword through which a combinator subschema,
// or one nested in it, declares params, and for "properties" the param it
// names that declared (the top-level "properties") lacks. Only constraints
// pass: "additionalProperties": false, and "properties" that names declared
// params alone, such as a "const" that tells "oneOf" branches apart.
func declaringKeyword(subschema json.RawMessage, declared map[string]json.RawMessage) (string, string) {
	var parsed map[string]json.RawMessage
	if json.Unmarshal(subschema, &parsed) != nil {
		return "", ""
	}
	for _, keyword := range declaringKeywords {
		raw, ok := parsed[keyword]
		if !ok || strings.TrimSpace(string(raw)) == "false" {
			continue
		}
		var named map[string]json.RawMessage
		if keyword == "properties" && json.Unmarshal(raw, &named) == nil && named != nil {
			if param := undeclaredParam(named, declared); param != "" {
				return keyword, param
			}
			continue
		}
		return keyword, ""
	}
	for _, nested := range combinatorSubschemas(parsed) {
		if keyword, param := declaringKeyword(nested.schema, declared); keyword != "" {
			return keyword, param
		}
	}
	return "", ""
}

// undeclaredParam returns the first name in named, in sorted order, that
// declared lacks, or "".
func undeclaredParam(named, declared map[string]json.RawMessage) string {
	names := make([]string, 0, len(named))
	for name := range named {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		if _, ok := declared[name]; !ok {
			return name
		}
	}
	return ""
}

// nonFlatParamsSchema describes why schema is not one flat object schema, or
// returns "" when it is (or is nil). Top-level combinators may only add
// constraints over the declared params. tako-server rejects such schemas too.
func nonFlatParamsSchema(schema json.RawMessage) string {
	if len(schema) == 0 {
		return ""
	}
	var parsed map[string]json.RawMessage
	if json.Unmarshal(schema, &parsed) != nil {
		return "is not a JSON object"
	}
	for _, keyword := range nonFlatSchemaKeywords {
		if _, ok := parsed[keyword]; ok {
			return "uses " + strconv.Quote(keyword)
		}
	}
	if kind, ok := parsed["type"]; ok && string(kind) != `"object"` {
		return "has type " + string(kind)
	}
	var declared map[string]json.RawMessage
	if properties, ok := parsed["properties"]; ok {
		if json.Unmarshal(properties, &declared) != nil {
			return "has \"properties\" that is not an object"
		}
	}
	for _, subschema := range combinatorSubschemas(parsed) {
		keyword, param := declaringKeyword(subschema.schema, declared)
		if param != "" {
			return "declares param " + strconv.Quote(param) + " through " + strconv.Quote(keyword) + " inside " + strconv.Quote(subschema.combinator) + " and not under the top-level \"properties\" (combinators may only add constraints over the params declared there)"
		}
		if keyword != "" {
			return "declares params through " + strconv.Quote(keyword) + " inside " + strconv.Quote(subschema.combinator) + " (combinators may only add constraints such as \"required\" over the top-level \"properties\")"
		}
	}
	return ""
}

func reservedParamName(schema json.RawMessage) string {
	var parsed struct {
		Properties map[string]json.RawMessage `json:"properties"`
	}
	if len(schema) == 0 || json.Unmarshal(schema, &parsed) != nil {
		return ""
	}
	for _, name := range ReservedChannelParamNames {
		if _, ok := parsed.Properties[name]; ok {
			return name
		}
	}
	return ""
}

// Clear removes all registered channel definitions.
func (r *ChannelRegistry) Clear() {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.definitions = map[string]ChannelDefinition{}
}

// Lookup returns a registered channel definition by name.
func (r *ChannelRegistry) Lookup(channel string) *ChannelDefinition {
	r.mu.RLock()
	defer r.mu.RUnlock()
	definition, ok := r.definitions[channel]
	if !ok {
		return nil
	}
	return &definition
}

// Authorize runs the registered authorization policy for a channel request.
func (r *ChannelRegistry) Authorize(input ChannelAuthorizeInput) (ChannelAuthorizeResponse, bool, bool) {
	definition := r.Lookup(input.Channel)
	if definition == nil {
		return ChannelAuthorizeResponse{}, false, false
	}

	if len(input.Params) == 0 {
		input.Params = json.RawMessage(`{}`)
	}

	if definition.Auth == nil {
		grant := ChannelGrant{ChannelLifecycleConfig: definition.ChannelLifecycleConfig.withDefaults()}
		return ChannelAuthorizeResponse{
			OK:           true,
			Transport:    definition.Transport,
			ChannelGrant: grant,
		}, true, true
	}
	if definition.Verify == nil {
		return ChannelAuthorizeResponse{OK: false}, true, false
	}

	decision := definition.Verify(VerifyInput{
		Channel:   input.Channel,
		Operation: input.Operation,
		Params:    input.Params,
		Header:    input.Header,
		Cookie:    input.Cookie,
	})
	if !decision.OK {
		return ChannelAuthorizeResponse{OK: false}, true, false
	}

	grant := decision.ChannelGrant
	lifecycle := definition.ChannelLifecycleConfig
	if grant.ReplayWindowMs != 0 && !lifecycle.Ephemeral {
		lifecycle.ReplayWindowMs = grant.ReplayWindowMs
	}
	if grant.InactivityTtlMs != 0 {
		lifecycle.InactivityTtlMs = grant.InactivityTtlMs
	}
	if grant.KeepaliveIntervalMs != 0 {
		lifecycle.KeepaliveIntervalMs = grant.KeepaliveIntervalMs
	}
	if grant.MaxConnectionLifetimeMs != 0 {
		lifecycle.MaxConnectionLifetimeMs = grant.MaxConnectionLifetimeMs
	}
	grant.ChannelLifecycleConfig = lifecycle.withDefaults()

	return ChannelAuthorizeResponse{
		OK:           true,
		Transport:    definition.Transport,
		ChannelGrant: grant,
	}, true, true
}

// Metadata returns sorted channel metadata for discovery.
func (r *ChannelRegistry) Metadata() []ChannelDefinitionMeta {
	r.mu.RLock()
	defer r.mu.RUnlock()

	names := make([]string, 0, len(r.definitions))
	for name := range r.definitions {
		names = append(names, name)
	}
	sort.Strings(names)

	out := make([]ChannelDefinitionMeta, 0, len(names))
	for _, name := range names {
		definition := r.definitions[name]
		auth := ChannelAuthMetadata{Public: true}
		if definition.Auth != nil {
			auth = ChannelAuthMetadata{Scheme: *definition.Auth}
		}
		out = append(out, ChannelDefinitionMeta{
			ChannelLifecycleConfig: definition.ChannelLifecycleConfig.withDefaults(),
			Channel:                name,
			ParamsSchema:           definition.ParamsSchema,
			Auth:                   auth,
			Transport:              definition.Transport,
		})
	}
	return out
}
