package internal

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"sync"

	"github.com/santhosh-tekuri/jsonschema/v5"
)

const (
	// DefaultChannelRetentionMs is the default replay window for channel events.
	DefaultChannelRetentionMs int64 = 24 * 60 * 60 * 1000
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
	if c.ReplayWindowMs == 0 {
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
// overrides for an accepted channel request.
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
	grant.ChannelLifecycleConfig = grant.ChannelLifecycleConfig.withDefaults()
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
	// ParamsSchema is a JSON Schema for query params.
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

// Register adds or replaces a channel definition.
func (r *ChannelRegistry) Register(name string, definition ChannelDefinition) {
	if definition.ParamsSchema == nil {
		definition.ParamsSchema = json.RawMessage(`{"type":"object"}`)
	}
	if definition.Auth != nil && definition.Auth.HeaderName == "" && definition.Auth.CookieName == "" {
		definition.Auth.HeaderName = "authorization"
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	r.definitions[name] = definition
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
	if grant.ChannelLifecycleConfig == (ChannelLifecycleConfig{}) {
		grant.ChannelLifecycleConfig = definition.ChannelLifecycleConfig
	}
	grant.ChannelLifecycleConfig = grant.ChannelLifecycleConfig.withDefaults()

	return ChannelAuthorizeResponse{
		OK:           true,
		Transport:    definition.Transport,
		ChannelGrant: grant,
	}, true, true
}

// ValidateParams validates and coerces channel query params against the
// registered JSON Schema.
func (r *ChannelRegistry) ValidateParams(channel string, query string) (json.RawMessage, error) {
	definition := r.Lookup(channel)
	if definition == nil {
		return nil, fmt.Errorf("channel %q is not registered", channel)
	}
	if len(definition.ParamsSchema) == 0 {
		return json.RawMessage(`{}`), nil
	}

	var schema map[string]any
	if err := json.Unmarshal(definition.ParamsSchema, &schema); err != nil {
		return nil, fmt.Errorf("invalid params schema: %w", err)
	}

	values, err := url.ParseQuery(query)
	if err != nil {
		return nil, err
	}
	params := make(map[string]any, len(values))
	for key, vals := range values {
		if len(vals) == 0 {
			continue
		}
		params[key] = coerceParamValue(schema, key, vals[len(vals)-1])
	}

	compiled, err := compileSchema(definition.ParamsSchema)
	if err != nil {
		return nil, err
	}
	if err := compiled.Validate(params); err != nil {
		return nil, err
	}

	b, err := json.Marshal(params)
	if err != nil {
		return nil, err
	}
	return json.RawMessage(b), nil
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
			Channel:      name,
			ParamsSchema: definition.ParamsSchema,
			Auth:         auth,
			Transport:    definition.Transport,
		})
	}
	return out
}

func compileSchema(raw json.RawMessage) (*jsonschema.Schema, error) {
	compiler := jsonschema.NewCompiler()
	compiler.Draft = jsonschema.Draft2020
	if err := compiler.AddResource("schema.json", bytes.NewReader(raw)); err != nil {
		return nil, err
	}
	return compiler.Compile("schema.json")
}

func coerceParamValue(schema map[string]any, key string, raw string) any {
	expected := propertyType(schema, key)
	switch expected {
	case "integer":
		if value, err := strconv.ParseInt(raw, 10, 64); err == nil {
			return value
		}
	case "number":
		if value, err := strconv.ParseFloat(raw, 64); err == nil {
			return value
		}
	case "boolean":
		if value, err := strconv.ParseBool(raw); err == nil {
			return value
		}
	}
	return raw
}

func propertyType(schema map[string]any, key string) string {
	properties, ok := schema["properties"].(map[string]any)
	if !ok {
		return ""
	}
	property, ok := properties[key].(map[string]any)
	if !ok {
		return ""
	}
	value, _ := property["type"].(string)
	return value
}
