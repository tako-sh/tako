package internal

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestChannelHeaderValueParseSplitsOnFirstSpace(t *testing.T) {
	t.Parallel()

	got := ParseChannelHeaderValue("Bearer abc 123")
	want := ChannelHeaderValue{Scheme: "Bearer", Value: "abc 123"}
	if got != want {
		t.Fatalf("got %#v, want %#v", got, want)
	}

	plain := ParseChannelHeaderValue("plain")
	if plain.Scheme != "" || plain.Value != "plain" {
		t.Fatalf("got %#v, want scheme='' value='plain'", plain)
	}
}

func TestVerifyInputJSONShape(t *testing.T) {
	t.Parallel()

	in := VerifyInput{
		Channel:   "chat",
		Operation: ChannelOperationSubscribe,
		Params:    json.RawMessage(`{"roomId":"r1"}`),
		Header:    &ChannelHeaderValue{Scheme: "Bearer", Value: "abc"},
	}
	b, err := json.Marshal(in)
	if err != nil {
		t.Fatal(err)
	}
	if !contains(string(b), `"header":{"scheme":"Bearer","value":"abc"}`) {
		t.Fatalf("missing header field: %s", b)
	}
	if contains(string(b), `"cookie"`) {
		t.Fatalf("cookie should be omitted when nil: %s", b)
	}
}

func TestChannelsRegisterAndLookup(t *testing.T) {
	t.Parallel()

	r := NewChannelRegistry()
	r.Register("chat", ChannelDefinition{
		ParamsSchema: []byte(`{"type":"object","properties":{"roomId":{"type":"string"}},"required":["roomId"]}`),
		Auth:         &ChannelAuthScheme{HeaderName: "authorization"},
		Verify:       func(VerifyInput) ChannelAuthDecision { return AllowChannel(ChannelGrant{Subject: "u1"}) },
	})
	if r.Lookup("chat") == nil {
		t.Fatal("expected chat to be present")
	}
	if r.Lookup("missing") != nil {
		t.Fatal("expected nil for unknown channel")
	}
}

func TestAuthorizeRunsVerifyAndReturnsSubject(t *testing.T) {
	t.Parallel()

	r := NewChannelRegistry()
	r.Register("chat", ChannelDefinition{
		ParamsSchema: []byte(`{"type":"object"}`),
		Auth:         &ChannelAuthScheme{HeaderName: "authorization"},
		Verify: func(in VerifyInput) ChannelAuthDecision {
			if in.Header == nil || in.Header.Scheme != "Bearer" {
				return RejectChannel()
			}
			return AllowChannel(ChannelGrant{Subject: "u1"})
		},
	})

	resp, defined, allowed := r.Authorize(ChannelAuthorizeInput{
		Channel:   "chat",
		Operation: ChannelOperationSubscribe,
		Params:    json.RawMessage(`{}`),
		Header:    &ChannelHeaderValue{Scheme: "Bearer", Value: "abc"},
	})
	if !defined || !allowed || !resp.OK || resp.Subject != "u1" {
		t.Fatalf("unexpected resp defined=%v allowed=%v %#v", defined, allowed, resp)
	}
}

func TestRegistryMetadataSerializesPublicAuthAsFalse(t *testing.T) {
	t.Parallel()

	r := NewChannelRegistry()
	r.Register("status", ChannelDefinition{
		ParamsSchema: []byte(`{"type":"object"}`),
	})
	meta := r.Metadata()
	b, err := json.Marshal(meta)
	if err != nil {
		t.Fatal(err)
	}
	if !contains(string(b), `"auth":false`) {
		t.Fatalf("expected public auth false in %s", b)
	}
}

func TestRegistryDefaultSchemaAcceptsNoParams(t *testing.T) {
	t.Parallel()

	r := NewChannelRegistry()
	r.Register("status", ChannelDefinition{})
	b, err := json.Marshal(r.Metadata())
	if err != nil {
		t.Fatal(err)
	}
	// Params select streams: tako-server rejects undeclared ones, and the
	// default schema says so.
	if !contains(string(b), `"paramsSchema":{"type":"object","additionalProperties":false}`) {
		t.Fatalf("expected a schema without params in %s", b)
	}
}

func TestAuthorizeInheritsDefinitionLifecycleForSubjectOnlyGrant(t *testing.T) {
	t.Parallel()
	r := NewChannelRegistry()
	r.Register("chat", ChannelDefinition{
		ChannelLifecycleConfig: ChannelLifecycleConfig{
			KeepaliveIntervalMs: 3000, MaxConnectionLifetimeMs: 60_000,
		},
		Auth: &ChannelAuthScheme{HeaderName: "authorization"},
		Verify: func(VerifyInput) ChannelAuthDecision {
			return AllowChannel(ChannelGrant{Subject: "user"})
		},
	})
	response, _, allowed := r.Authorize(ChannelAuthorizeInput{Channel: "chat", Operation: ChannelOperationConnect})
	if !allowed || response.MaxConnectionLifetimeMs != 60_000 || response.KeepaliveIntervalMs != 3000 {
		t.Fatalf("definition lifecycle should apply: %#v", response)
	}
}

func TestAuthorizeMergesExplicitLifecycleOverrides(t *testing.T) {
	t.Parallel()
	r := NewChannelRegistry()
	r.Register("chat", ChannelDefinition{
		ChannelLifecycleConfig: ChannelLifecycleConfig{
			ReplayWindowMs: 90_000, InactivityTtlMs: 30_000,
			KeepaliveIntervalMs: 3000, MaxConnectionLifetimeMs: 60_000,
		},
		Auth: &ChannelAuthScheme{HeaderName: "authorization"},
		Verify: func(VerifyInput) ChannelAuthDecision {
			return AllowChannel(ChannelGrant{Subject: "user", ChannelLifecycleConfig: ChannelLifecycleConfig{
				KeepaliveIntervalMs: 1000,
			}})
		},
	})
	response, _, allowed := r.Authorize(ChannelAuthorizeInput{Channel: "chat", Operation: ChannelOperationConnect})
	if !allowed || response.MaxConnectionLifetimeMs != 60_000 || response.KeepaliveIntervalMs != 1000 || response.ReplayWindowMs != 90_000 || response.InactivityTtlMs != 30_000 {
		t.Fatalf("only explicitly supplied lifecycle fields should override: %#v", response)
	}
}

func contains(haystack, needle string) bool {
	if len(needle) == 0 {
		return true
	}
	for i := 0; i+len(needle) <= len(haystack); i++ {
		if haystack[i:i+len(needle)] == needle {
			return true
		}
	}
	return false
}

func TestEphemeralChannelMetadataAndAuthorization(t *testing.T) {
	r := NewChannelRegistry()
	r.Register("live", ChannelDefinition{ChannelLifecycleConfig: ChannelLifecycleConfig{Ephemeral: true}})
	for _, private := range []bool{false, true} {
		definition := *r.Lookup("live")
		if private {
			definition.Auth = &ChannelAuthScheme{}
			definition.Verify = func(VerifyInput) ChannelAuthDecision {
				return AllowChannel(ChannelGrant{Subject: "user", ChannelLifecycleConfig: ChannelLifecycleConfig{ReplayWindowMs: 1000}})
			}
			r.Register("live", definition)
		}
		response, found, allowed := r.Authorize(ChannelAuthorizeInput{Channel: "live", Operation: ChannelOperationSubscribe})
		if !found || !allowed || !response.Ephemeral || response.ReplayWindowMs != 0 {
			t.Fatalf("unexpected response: %+v", response)
		}
		encoded, err := json.Marshal(response)
		if err != nil {
			t.Fatal(err)
		}
		var fields map[string]any
		if err := json.Unmarshal(encoded, &fields); err != nil {
			t.Fatal(err)
		}
		if _, ok := fields["replayWindowMs"]; ok {
			t.Fatalf("ephemeral response contains replay: %s", encoded)
		}
	}
	if !r.Metadata()[0].Ephemeral || r.Metadata()[0].ReplayWindowMs != 0 {
		t.Fatal("metadata lost ephemeral mode")
	}
}

func TestEphemeralChannelRejectsReplayWindow(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatal("expected invalid channel registration to panic")
		}
	}()
	NewChannelRegistry().Register("invalid", ChannelDefinition{ChannelLifecycleConfig: ChannelLifecycleConfig{Ephemeral: true, ReplayWindowMs: 10}})
}

func TestChannelRegistrationRejectsReservedParamNames(t *testing.T) {
	for _, name := range []string{"from", "last_message_id"} {
		func() {
			defer func() {
				message, _ := recover().(string)
				if !strings.Contains(message, `declares param "`+name+`"`) {
					t.Fatalf("expected reserved param %q to panic, got %q", name, message)
				}
			}()
			NewChannelRegistry().Register("reply", ChannelDefinition{
				ParamsSchema: json.RawMessage(`{"type":"object","properties":{"` + name + `":{"type":"string"}}}`),
			})
		}()
	}
	// Stores carry params as JSON, not query values.
	NewChannelRegistry().Register(storeChannelPrefix+"notes", ChannelDefinition{
		ParamsSchema: json.RawMessage(`{"type":"object","properties":{"from":{"type":"string"}}}`),
	})
}
