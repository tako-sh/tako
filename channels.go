package tako

import (
	"tako.sh/internal"
)

// ChannelTransport identifies the live transport used by a channel.
type ChannelTransport = internal.ChannelTransport

// ChannelOperation is the operation being authorized for a channel request.
type ChannelOperation = internal.ChannelOperation

// ChannelLifecycleConfig controls replay, idle, keepalive, and max lifetime
// behavior for channel subscriptions.
type ChannelLifecycleConfig = internal.ChannelLifecycleConfig

// ChannelHeaderValue is a parsed authorization header value.
type ChannelHeaderValue = internal.ChannelHeaderValue

// ChannelAuthScheme describes where channel credentials are read from.
type ChannelAuthScheme = internal.ChannelAuthScheme

// VerifyInput is passed to a channel authorization function.
type VerifyInput = internal.VerifyInput

// ChannelGrant describes the authorized subject and optional lifecycle
// overrides for an accepted channel request.
type ChannelGrant = internal.ChannelGrant

// ChannelAuthDecision is the result returned by a channel authorization
// function.
type ChannelAuthDecision = internal.ChannelAuthDecision

// ChannelDefinition describes a channel registered with [Channels].
type ChannelDefinition = internal.ChannelDefinition

// ChannelAuthorizeInput is the internal authorization request shape used by
// Tako channel endpoints.
type ChannelAuthorizeInput = internal.ChannelAuthorizeInput

// ChannelAuthorizeResponse is the internal authorization response shape used by
// Tako channel endpoints.
type ChannelAuthorizeResponse = internal.ChannelAuthorizeResponse

// ChannelDefinitionMeta is the public metadata shape returned by the channel
// registry endpoint.
type ChannelDefinitionMeta = internal.ChannelDefinitionMeta

// ChannelRegistry stores channel definitions for discovery and authorization.
type ChannelRegistry = internal.ChannelRegistry

const (
	// ChannelTransportWS enables WebSocket channels.
	ChannelTransportWS = internal.ChannelTransportWS

	// ChannelOperationSubscribe authorizes a channel subscription.
	ChannelOperationSubscribe = internal.ChannelOperationSubscribe
	// ChannelOperationPublish authorizes a server-side publish.
	ChannelOperationPublish = internal.ChannelOperationPublish
	// ChannelOperationConnect authorizes a WebSocket connection.
	ChannelOperationConnect = internal.ChannelOperationConnect
)

// Channels is the process-local channel registry used by Tako endpoints.
var Channels = internal.Channels

// AllowChannel accepts a channel authorization request.
func AllowChannel(grant ChannelGrant) ChannelAuthDecision {
	return internal.AllowChannel(grant)
}

// RejectChannel denies a channel authorization request.
func RejectChannel() ChannelAuthDecision {
	return internal.RejectChannel()
}

// ParseChannelHeaderValue splits an authorization header into scheme and value.
func ParseChannelHeaderValue(raw string) ChannelHeaderValue {
	return internal.ParseChannelHeaderValue(raw)
}
