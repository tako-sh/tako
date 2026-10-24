package tako

import (
	"encoding/json"

	"tako.sh/internal"
)

// PresenceDefinition configures a named presence space. Register it before
// calling ListenAndServe. Its authorization policy guards membership and the
// member list; validated params partition spaces such as private chats.
type PresenceDefinition = internal.PresenceDefinition

// RegisterPresence adds or replaces a presence space. Presence uses WebSocket
// transport and a separate namespace from channels with the same name.
func RegisterPresence(name string, definition PresenceDefinition) {
	internal.RegisterPresence(name, definition)
}

// PresenceMember describes one connection in a presence snapshot. Separate tabs
// have separate connection IDs even when they share an authenticated subject.
type PresenceMember struct {
	ConnectionID string `json:"connectionId"`
	Subject      string `json:"subject,omitempty"`
	// Data is untrusted client-provided metadata; validate it before use.
	Data json.RawMessage `json:"data"`
}

// PresenceSnapshot is the server-authored current member list. It is ephemeral
// and has no durable channel replay ID.
type PresenceSnapshot struct {
	Type         string           `json:"type"`
	ConnectionID string           `json:"connectionId"`
	Members      []PresenceMember `json:"members"`
}
