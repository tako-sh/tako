package internal

import "encoding/json"

const presenceChannelPrefix = "tako.presence:"

// PresenceDefinition configures membership in a named presence space.
// ParamsSchema scopes membership by validated params; Auth and Verify guard
// access to that scope before its members are returned. Like a channel's, it
// must be one object schema with flat params under its top-level
// "properties", and its properties cannot be named "from" or
// "last_message_id" (reserved on channel routes).
type PresenceDefinition struct {
	ParamsSchema json.RawMessage
	Auth         *ChannelAuthScheme
	Verify       func(VerifyInput) ChannelAuthDecision
	// KeepaliveIntervalMs controls WebSocket keepalive pings.
	KeepaliveIntervalMs int64
	// MaxConnectionLifetimeMs bounds the time before a connection reauthorizes.
	MaxConnectionLifetimeMs int64
}

// RegisterPresence adds or replaces a presence definition in the process-local
// registry served by Tako's authenticated internal endpoints.
func RegisterPresence(name string, definition PresenceDefinition) {
	verify := definition.Verify
	if verify != nil {
		verify = func(input VerifyInput) ChannelAuthDecision {
			input.Channel = name
			return definition.Verify(input)
		}
	}
	Channels.Register(presenceChannelPrefix+name, ChannelDefinition{
		ParamsSchema: definition.ParamsSchema,
		Auth:         definition.Auth,
		Verify:       verify,
		Transport:    ChannelTransportWS,
		ChannelLifecycleConfig: ChannelLifecycleConfig{
			KeepaliveIntervalMs:     definition.KeepaliveIntervalMs,
			MaxConnectionLifetimeMs: definition.MaxConnectionLifetimeMs,
		},
	})
}
