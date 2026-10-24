package tako

import (
	"encoding/json"
	"testing"
)

func TestPresencePublicAPI(t *testing.T) {
	Channels.Clear()
	defer Channels.Clear()
	RegisterPresence("lobby", PresenceDefinition{})
	definition := Channels.Lookup("tako.presence:lobby")
	if definition == nil || definition.Transport != ChannelTransportWS {
		t.Fatalf("presence must register a WebSocket definition: %#v", definition)
	}
	var snapshot PresenceSnapshot
	if err := json.Unmarshal([]byte(`{"type":"tako.presence","connectionId":"tab","members":[{"connectionId":"tab","subject":"user","data":{"typing":true}}]}`), &snapshot); err != nil {
		t.Fatal(err)
	}
	if snapshot.Type != "tako.presence" || snapshot.ConnectionID != "tab" || len(snapshot.Members) != 1 || snapshot.Members[0].Subject != "user" {
		t.Fatalf("unexpected snapshot: %#v", snapshot)
	}
	var data struct {
		Typing bool `json:"typing"`
	}
	if err := json.Unmarshal(snapshot.Members[0].Data, &data); err != nil || !data.Typing {
		t.Fatalf("unexpected member metadata: %s", snapshot.Members[0].Data)
	}
	anonymous, err := json.Marshal(PresenceMember{ConnectionID: "guest", Data: json.RawMessage(`{}`)})
	if err != nil {
		t.Fatal(err)
	}
	var fields map[string]any
	if err := json.Unmarshal(anonymous, &fields); err != nil {
		t.Fatal(err)
	}
	if fields["connectionId"] != "guest" || fields["data"] == nil {
		t.Fatalf("unexpected anonymous member: %s", anonymous)
	}
}
