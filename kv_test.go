package tako

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
	"time"

	"tako.sh/internal"
)

func TestGetKVReadsBootstrapBindings(t *testing.T) {
	previous := bootstrap
	t.Cleanup(func() { bootstrap = previous })
	bootstrap = &internal.Bootstrap{
		Token:   "tok",
		Secrets: map[string]string{},
		KV: map[string]internal.KVConnection{
			"sessions": {Token: "secret-token"},
			"cache":    {Token: "other"},
		},
	}

	sessions, ok := GetKV("sessions")
	if !ok || sessions.Name() != "sessions" {
		t.Fatalf("GetKV(sessions) = %v, %v", sessions, ok)
	}
	encoded, err := json.Marshal(map[string]*KV{"sessions": sessions})
	if err != nil {
		t.Fatalf("json.Marshal: %v", err)
	}
	for _, text := range []string{sessions.String(), fmt.Sprintf("%v", sessions), fmt.Sprintf("%+v", sessions), fmt.Sprintf("%#v", sessions), string(encoded)} {
		if strings.Contains(text, "secret-token") {
			t.Fatalf("formatted KV store leaks its token: %s", text)
		}
	}
	if string(encoded) != `{"sessions":{"kv":"sessions"}}` {
		t.Fatalf("encoded KV store = %s", encoded)
	}
	if _, ok := GetKV("missing"); ok {
		t.Fatal("GetKV(missing) must report false")
	}
	if got := KVNames(); strings.Join(got, ",") != "cache,sessions" {
		t.Fatalf("KVNames() = %v", got)
	}
}

func TestKVRejectsInvalidRequestsBeforeSending(t *testing.T) {
	socket := startWireSocket(t)
	ctx := context.Background()
	store := &KV{name: "sessions", token: "t"}
	var value any
	if _, err := store.Get(ctx, "", &value); err == nil || !strings.Contains(err.Error(), "empty") {
		t.Fatalf("an empty key must be rejected: %v", err)
	}
	if err := store.Delete(ctx, strings.Repeat("k", 1025)); err == nil || !strings.Contains(err.Error(), "1 KiB") {
		t.Fatalf("a long key must be rejected: %v", err)
	}
	if err := store.Put(ctx, "k", strings.Repeat("x", 256*1024)); err == nil || !strings.Contains(err.Error(), "256 KiB") {
		t.Fatalf("a large value must be rejected: %v", err)
	}
	if err := store.Put(ctx, "k", func() {}); err == nil {
		t.Fatal("a value JSON cannot encode must be rejected")
	}
	for _, ttl := range []time.Duration{-time.Second, 0} {
		if err := store.Put(ctx, "k", 1, WithTTL(ttl)); err == nil {
			t.Fatalf("a TTL of %v must be rejected", ttl)
		}
	}
	socket.mu.Lock()
	defer socket.mu.Unlock()
	if len(socket.received) != 0 {
		t.Fatalf("invalid requests were sent: %d", len(socket.received))
	}
}

func TestKVRoundsTTLsUpToWholeMilliseconds(t *testing.T) {
	socket := startWireSocket(t)
	store := &KV{name: "sessions", token: "t"}
	if err := store.Put(context.Background(), "k", 1, WithTTL(1500*time.Microsecond)); err != nil {
		t.Fatal(err)
	}
	if got := socket.take()["ttl_ms"]; got != float64(2) {
		t.Fatalf("ttl_ms = %v", got)
	}
}

func TestKVRequiresTheRuntime(t *testing.T) {
	t.Setenv("TAKO_INTERNAL_SOCKET", "")
	t.Setenv("TAKO_APP_NAME", "")
	store := &KV{name: "sessions", token: "t"}
	var value any
	if _, err := store.Get(context.Background(), "k", &value); err == nil {
		t.Fatal("KV stores need the internal socket")
	}
}

func TestGetKVWithoutBootstrap(t *testing.T) {
	previous := bootstrap
	t.Cleanup(func() { bootstrap = previous })
	bootstrap = nil
	if _, ok := GetKV("sessions"); ok {
		t.Fatal("no bootstrap means no KV stores")
	}
	if KVNames() != nil {
		t.Fatal("no bootstrap means no names")
	}
}
