package internal

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestRegisterPresenceSharesChannelDiscoveryAndAuthorization(t *testing.T) {
	Channels.Clear()
	defer Channels.Clear()
	Channels.Register("chat", ChannelDefinition{})
	RegisterPresence("chat", PresenceDefinition{
		ParamsSchema:            json.RawMessage(`{"type":"object","properties":{"chatId":{"type":"string"}},"required":["chatId"]}`),
		Auth:                    &ChannelAuthScheme{},
		MaxConnectionLifetimeMs: 60_000,
		Verify: func(input VerifyInput) ChannelAuthDecision {
			if input.Channel != "chat" || input.Operation != ChannelOperationConnect {
				t.Fatalf("unexpected verification context: %#v", input)
			}
			var params struct {
				ChatID string `json:"chatId"`
			}
			if err := json.Unmarshal(input.Params, &params); err != nil {
				t.Fatal(err)
			}
			if input.Header == nil || input.Header.Value != "valid" || params.ChatID != "allowed" {
				return RejectChannel()
			}
			return AllowChannel(ChannelGrant{Subject: "verified-user"})
		},
	})
	if Channels.Lookup("chat") == nil || Channels.Lookup("tako.presence:chat") == nil {
		t.Fatal("channel and presence should have separate namespaces")
	}
	metadata := Channels.Metadata()
	if len(metadata) != 2 || metadata[1].Channel != "tako.presence:chat" || metadata[1].Transport != ChannelTransportWS || metadata[1].Auth.Scheme.HeaderName != "authorization" {
		t.Fatalf("unexpected discovery metadata: %#v", metadata)
	}
	response, defined, allowed := Channels.Authorize(ChannelAuthorizeInput{
		Channel: "tako.presence:chat", Operation: ChannelOperationConnect,
		Params: json.RawMessage(`{"chatId":"allowed"}`), Header: &ChannelHeaderValue{Value: "valid"},
	})
	if !defined || !allowed || response.Subject != "verified-user" || response.Transport != ChannelTransportWS || response.MaxConnectionLifetimeMs != 60_000 {
		t.Fatalf("unexpected presence authorization: %#v", response)
	}
	_, _, allowed = Channels.Authorize(ChannelAuthorizeInput{
		Channel: "tako.presence:chat", Operation: ChannelOperationConnect,
		Params: json.RawMessage(`{"chatId":"private"}`), Header: &ChannelHeaderValue{Value: "valid"},
	})
	if allowed {
		t.Fatal("presence should reject access to another chat")
	}
}

func TestPresenceSupportsPublicAndCookieAuth(t *testing.T) {
	Channels.Clear()
	defer Channels.Clear()
	RegisterPresence("public", PresenceDefinition{})
	response, _, allowed := Channels.Authorize(ChannelAuthorizeInput{Channel: "tako.presence:public", Operation: ChannelOperationConnect})
	if !allowed || !response.OK || response.Subject != "" {
		t.Fatalf("unexpected anonymous grant: %#v", response)
	}
	RegisterPresence("private", PresenceDefinition{
		Auth: &ChannelAuthScheme{CookieName: "session"},
		Verify: func(input VerifyInput) ChannelAuthDecision {
			if input.Cookie == nil || *input.Cookie != "valid" {
				return RejectChannel()
			}
			return AllowChannel(ChannelGrant{Subject: "cookie-user"})
		},
	})
	cookie := "valid"
	response, _, allowed = Channels.Authorize(ChannelAuthorizeInput{Channel: "tako.presence:private", Operation: ChannelOperationConnect, Cookie: &cookie})
	if !allowed || response.Subject != "cookie-user" {
		t.Fatalf("unexpected cookie grant: %#v", response)
	}
	if Channels.Lookup("tako.presence:private").Auth.HeaderName != "" {
		t.Fatal("cookie-only auth should not require a header")
	}
}

func TestPresenceUsesAuthenticatedInternalEndpoints(t *testing.T) {
	Channels.Clear()
	defer Channels.Clear()
	RegisterPresence("lobby", PresenceDefinition{
		Auth: &ChannelAuthScheme{},
		Verify: func(input VerifyInput) ChannelAuthDecision {
			if input.Header == nil || input.Header.Value != "valid" {
				return RejectChannel()
			}
			return AllowChannel(ChannelGrant{Subject: "member"})
		},
	})
	handler := NewEndpointHandler("demo", "instance", "version", "internal-secret", http.NotFoundHandler())
	for _, tc := range []struct {
		credential, token string
		status            int
	}{
		{"valid", "", http.StatusUnauthorized},
		{"invalid", "internal-secret", http.StatusForbidden},
		{"valid", "internal-secret", http.StatusOK},
	} {
		request := httptest.NewRequest(http.MethodPost, "/channels/authorize", strings.NewReader(
			`{"channel":"tako.presence:lobby","operation":"connect","params":{},"header":{"value":"`+tc.credential+`"}}`))
		request.Host = "demo.tako"
		request.Header.Set("x-tako-internal-token", tc.token)
		recorder := httptest.NewRecorder()
		handler.ServeHTTP(recorder, request)
		if recorder.Code != tc.status {
			t.Fatalf("status = %d, want %d", recorder.Code, tc.status)
		}
		if recorder.Code == http.StatusOK {
			var grant ChannelAuthorizeResponse
			if err := json.Unmarshal(recorder.Body.Bytes(), &grant); err != nil {
				t.Fatal(err)
			}
			if grant.Subject != "member" || grant.Transport != ChannelTransportWS {
				t.Fatalf("unexpected grant: %#v", grant)
			}
		}
	}
}
