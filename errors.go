package tako

import "tako.sh/internal"

// Rejections a Tako app may need to branch on. Match them with errors.Is: a
// *ServerError whose Code names one of them reports true. Rejections without
// a code match none of them.
var (
	// ErrRunNotOwned: a worker write named a run this attempt no longer owns.
	ErrRunNotOwned = internal.ErrRunNotOwned
	// ErrRunIDConflict: an enqueue ID belongs to a different workflow or payload.
	ErrRunIDConflict = internal.ErrRunIDConflict
	// ErrInvalidRequest: the request or its arguments are malformed; retrying cannot succeed.
	ErrInvalidRequest = internal.ErrInvalidRequest
	// ErrChannelUnknown: the channel is not defined by this build or by the recipient app.
	ErrChannelUnknown = internal.ErrChannelUnknown
	// ErrChannelParamsInvalid: channel params are malformed or fail the channel's params schema.
	ErrChannelParamsInvalid = internal.ErrChannelParamsInvalid
	// ErrChannelClosed: the channel stream was closed and accepts no more messages.
	ErrChannelClosed = internal.ErrChannelClosed
	// ErrUnavailable: the server is draining, overloaded, or its storage is unreachable; retry later.
	ErrUnavailable = internal.ErrUnavailable
)

// ServerError is a request the Tako server rejected. Its Code is the
// machine-readable reason, or empty when the server sent none.
type ServerError = internal.ServerError
