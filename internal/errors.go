package internal

import "errors"

// Rejections a Tako app may need to branch on. Match them with errors.Is: a
// *ServerError whose Code names one of them reports true. Rejections without
// a code match none of them.
var (
	// ErrRunNotOwned: a worker write named a run this attempt no longer owns.
	ErrRunNotOwned = errors.New("tako: run is no longer owned by this worker")
	// ErrRunIDConflict: an enqueue ID belongs to a different workflow or payload.
	ErrRunIDConflict = errors.New("tako: workflow run ID belongs to a different workflow or payload")
	// ErrInvalidRequest: the request or its arguments are malformed; retrying cannot succeed.
	ErrInvalidRequest = errors.New("tako: invalid request")
	// ErrChannelUnknown: the channel is not defined by this build or by the recipient app.
	ErrChannelUnknown = errors.New("tako: channel is not defined")
	// ErrChannelParamsInvalid: channel params are malformed or fail the channel's params schema.
	ErrChannelParamsInvalid = errors.New("tako: invalid channel params")
	// ErrChannelClosed: the channel stream was closed and accepts no more messages.
	ErrChannelClosed = errors.New("tako: channel stream is closed")
	// ErrUnavailable: the server is draining, overloaded, or its storage is unreachable; retry later.
	ErrUnavailable = errors.New("tako: server unavailable")
)

var errorCodes = map[string]error{
	"run_not_owned":          ErrRunNotOwned,
	"run_id_conflict":        ErrRunIDConflict,
	"invalid_request":        ErrInvalidRequest,
	"channel_unknown":        ErrChannelUnknown,
	"channel_params_invalid": ErrChannelParamsInvalid,
	"channel_closed":         ErrChannelClosed,
	"unavailable":            ErrUnavailable,
}

// ServerError is a request the Tako server rejected.
type ServerError struct {
	// Code is the machine-readable reason, or empty when the server sent none.
	Code string
	// Message is the server's free-text diagnostic.
	Message string
}

func (e *ServerError) Error() string { return "tako: " + e.Message }

// Is reports whether target is the sentinel error for e.Code.
func (e *ServerError) Is(target error) bool {
	sentinel, ok := errorCodes[e.Code]
	return ok && sentinel == target
}
