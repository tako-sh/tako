package tako

import (
	"context"
	"encoding/json"
	"testing"
)

func TestStorePublicAPI(t *testing.T) {
	Channels.Clear()
	defer Channels.Clear()
	var load func(context.Context, StoreLoadInput) (any, error) = func(_ context.Context, input StoreLoadInput) (any, error) {
		return map[string]string{"subject": input.Subject}, nil
	}
	var handle *Store = RegisterStore("messages", StoreDefinition{
		ParamsSchema: json.RawMessage(`{"type":"object"}`),
		Load:         load,
		Offline:      &StoreOfflineConfig{MaxAgeMs: 60_000, Version: "1"},
	})
	if handle.Name() != "messages" {
		t.Fatalf("unexpected handle name: %s", handle.Name())
	}
	definition := Channels.Lookup("tako.store:messages")
	if definition == nil || definition.Transport != "" {
		t.Fatalf("store must register a read-only SSE definition: %#v", definition)
	}
}
