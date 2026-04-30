package tako

import (
	"tako.sh/internal"
)

type ChannelTransport = internal.ChannelTransport
type ChannelOperation = internal.ChannelOperation
type ChannelLifecycleConfig = internal.ChannelLifecycleConfig
type ChannelHeaderValue = internal.ChannelHeaderValue
type ChannelAuthScheme = internal.ChannelAuthScheme
type VerifyInput = internal.VerifyInput
type ChannelGrant = internal.ChannelGrant
type ChannelAuthDecision = internal.ChannelAuthDecision
type ChannelDefinition = internal.ChannelDefinition
type ChannelAuthorizeInput = internal.ChannelAuthorizeInput
type ChannelAuthorizeResponse = internal.ChannelAuthorizeResponse
type ChannelDefinitionMeta = internal.ChannelDefinitionMeta
type ChannelRegistry = internal.ChannelRegistry

const (
	ChannelTransportWS = internal.ChannelTransportWS

	ChannelOperationSubscribe = internal.ChannelOperationSubscribe
	ChannelOperationPublish   = internal.ChannelOperationPublish
	ChannelOperationConnect   = internal.ChannelOperationConnect
)

var Channels = internal.Channels

func AllowChannel(grant ChannelGrant) ChannelAuthDecision {
	return internal.AllowChannel(grant)
}

func RejectChannel() ChannelAuthDecision {
	return internal.RejectChannel()
}

func ParseChannelHeaderValue(raw string) ChannelHeaderValue {
	return internal.ParseChannelHeaderValue(raw)
}
