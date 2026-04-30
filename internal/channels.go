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
	DefaultChannelRetentionMs             int64 = 24 * 60 * 60 * 1000
	DefaultChannelInactivityTtlMs         int64 = 0
	DefaultChannelKeepaliveIntervalMs     int64 = 25 * 1000
	DefaultChannelMaxConnectionLifetimeMs int64 = 2 * 60 * 60 * 1000
)

type ChannelTransport string

const (
	ChannelTransportWS ChannelTransport = "ws"
)

type ChannelOperation string

const (
	ChannelOperationSubscribe ChannelOperation = "subscribe"
	ChannelOperationPublish   ChannelOperation = "publish"
	ChannelOperationConnect   ChannelOperation = "connect"
)

type ChannelLifecycleConfig struct {
	ReplayWindowMs          int64 `json:"replayWindowMs,omitempty"`
	InactivityTtlMs         int64 `json:"inactivityTtlMs"`
	KeepaliveIntervalMs     int64 `json:"keepaliveIntervalMs,omitempty"`
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

type ChannelHeaderValue struct {
	Scheme string `json:"scheme,omitempty"`
	Value  string `json:"value"`
}

func ParseChannelHeaderValue(raw string) ChannelHeaderValue {
	if idx := strings.IndexByte(raw, ' '); idx >= 0 {
		return ChannelHeaderValue{Scheme: raw[:idx], Value: raw[idx+1:]}
	}
	return ChannelHeaderValue{Value: raw}
}

type ChannelAuthScheme struct {
	HeaderName string `json:"headerName,omitempty"`
	CookieName string `json:"cookieName,omitempty"`
}

type VerifyInput struct {
	Channel   string              `json:"channel"`
	Operation ChannelOperation    `json:"operation"`
	Params    json.RawMessage     `json:"params"`
	Header    *ChannelHeaderValue `json:"header,omitempty"`
	Cookie    *string             `json:"cookie,omitempty"`
}

type ChannelGrant struct {
	Subject string `json:"subject,omitempty"`
	ChannelLifecycleConfig
}

type ChannelAuthDecision struct {
	OK bool `json:"ok"`
	ChannelGrant
}

func AllowChannel(grant ChannelGrant) ChannelAuthDecision {
	grant.ChannelLifecycleConfig = grant.ChannelLifecycleConfig.withDefaults()
	return ChannelAuthDecision{
		OK:           true,
		ChannelGrant: grant,
	}
}

func RejectChannel() ChannelAuthDecision {
	return ChannelAuthDecision{OK: false}
}

type ChannelDefinition struct {
	ChannelLifecycleConfig
	ParamsSchema json.RawMessage
	Auth         *ChannelAuthScheme
	Verify       func(VerifyInput) ChannelAuthDecision
	Transport    ChannelTransport
}

type ChannelAuthorizeInput struct {
	Channel   string              `json:"channel"`
	Operation ChannelOperation    `json:"operation"`
	Params    json.RawMessage     `json:"params"`
	Header    *ChannelHeaderValue `json:"header,omitempty"`
	Cookie    *string             `json:"cookie,omitempty"`
}

type ChannelAuthorizeResponse struct {
	OK        bool             `json:"ok"`
	Transport ChannelTransport `json:"transport,omitempty"`
	ChannelGrant
}

type ChannelAuthMetadata struct {
	Public bool
	Scheme ChannelAuthScheme
}

func (m ChannelAuthMetadata) MarshalJSON() ([]byte, error) {
	if m.Public {
		return []byte("false"), nil
	}
	return json.Marshal(m.Scheme)
}

type ChannelDefinitionMeta struct {
	Channel      string              `json:"channel"`
	ParamsSchema json.RawMessage     `json:"paramsSchema"`
	Auth         ChannelAuthMetadata `json:"auth"`
	Transport    ChannelTransport    `json:"transport,omitempty"`
}

type ChannelRegistry struct {
	mu          sync.RWMutex
	definitions map[string]ChannelDefinition
}

func NewChannelRegistry() *ChannelRegistry {
	return &ChannelRegistry{definitions: map[string]ChannelDefinition{}}
}

var Channels = NewChannelRegistry()

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

func (r *ChannelRegistry) Clear() {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.definitions = map[string]ChannelDefinition{}
}

func (r *ChannelRegistry) Lookup(channel string) *ChannelDefinition {
	r.mu.RLock()
	defer r.mu.RUnlock()
	definition, ok := r.definitions[channel]
	if !ok {
		return nil
	}
	return &definition
}

func (r *ChannelRegistry) ResolveDefinition(channel string) *ChannelDefinition {
	return r.Lookup(channel)
}

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
