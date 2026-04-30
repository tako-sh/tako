package internal

import (
	"encoding/json"
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

func TestValidateParamsCoercesAndChecks(t *testing.T) {
	t.Parallel()

	r := NewChannelRegistry()
	r.Register("chat", ChannelDefinition{
		ParamsSchema: []byte(`{"type":"object","properties":{"roomId":{"type":"string","minLength":1},"limit":{"type":"integer"}},"required":["roomId"]}`),
	})

	v, err := r.ValidateParams("chat", "roomId=r1&limit=10")
	if err != nil {
		t.Fatal(err)
	}
	var got map[string]any
	if err := json.Unmarshal(v, &got); err != nil {
		t.Fatal(err)
	}
	if got["roomId"] != "r1" || got["limit"].(float64) != 10 {
		t.Fatalf("got %#v", got)
	}

	if _, err := r.ValidateParams("chat", "limit=10"); err == nil {
		t.Fatal("expected error for missing required roomId")
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
