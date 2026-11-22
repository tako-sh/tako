package tako

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"tako.sh/internal"
)

func TestGetKVReadsBootstrapBindings(t *testing.T) {
	previous := bootstrap
	t.Cleanup(func() { bootstrap = previous })
	bootstrap = &internal.Bootstrap{
		Token:   "tok",
		Secrets: map[string]string{},
		KV: map[string]internal.KVConnection{
			"sessions": {URL: "redis://:secret-token@127.0.0.1:9846"},
			"cache":    {URL: "redis://:other@127.0.0.1:9846"},
		},
	}

	sessions, ok := GetKV("sessions")
	if !ok || sessions.URL != "redis://:secret-token@127.0.0.1:9846" {
		t.Fatalf("GetKV(sessions) = %v, %v", sessions, ok)
	}
	encoded, err := json.Marshal(map[string]KV{"sessions": sessions})
	if err != nil {
		t.Fatalf("json.Marshal: %v", err)
	}
	for _, text := range []string{sessions.String(), fmt.Sprintf("%v", sessions), fmt.Sprintf("%+v", sessions), fmt.Sprintf("%#v", sessions), string(encoded)} {
		if strings.Contains(text, "secret-token") {
			t.Fatalf("formatted KV store leaks its token: %s", text)
		}
	}
	if string(encoded) != `{"sessions":{"url":"redis://:[REDACTED]@127.0.0.1:9846"}}` {
		t.Fatalf("encoded KV store = %s", encoded)
	}
	if _, ok := GetKV("missing"); ok {
		t.Fatal("GetKV(missing) must report false")
	}
	if got := KVNames(); strings.Join(got, ",") != "cache,sessions" {
		t.Fatalf("KVNames() = %v", got)
	}
}

func TestKVRedactsUnexpectedURLs(t *testing.T) {
	for raw, want := range map[string]string{
		"tok-odd":             "[REDACTED]",
		"redis://:a@b@host:1": "redis://:[REDACTED]@host:1",
	} {
		if got := redactKVURL(raw); got != want {
			t.Fatalf("redactKVURL(%q) = %q, want %q", raw, got, want)
		}
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
