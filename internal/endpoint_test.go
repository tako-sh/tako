package internal

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
)

func TestStatusEndpoint(t *testing.T) {
	handler := NewEndpointHandler("demo/production", "test1234", "v1.0", "", http.NotFoundHandler())

	req := httptest.NewRequest(http.MethodGet, "/status", nil)
	req.Host = "demo.tako"
	w := httptest.NewRecorder()

	handler.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("status code = %d, want 200", w.Code)
	}

	var resp StatusResponse
	json.NewDecoder(w.Body).Decode(&resp)

	if resp.Status != "healthy" {
		t.Errorf("status = %q, want %q", resp.Status, "healthy")
	}
	if resp.InstanceID != "test1234" {
		t.Errorf("instance_id = %q, want %q", resp.InstanceID, "test1234")
	}
	if resp.PID != os.Getpid() {
		t.Errorf("pid = %d, want %d", resp.PID, os.Getpid())
	}
}

func TestInternalHostUsesBaseAppSegment(t *testing.T) {
	handler := NewEndpointHandler("demo/production", "test1234", "v1.0", "", http.NotFoundHandler())

	req := httptest.NewRequest(http.MethodGet, "/status", nil)
	req.Host = "demo-production.tako"
	w := httptest.NewRecorder()
	handler.ServeHTTP(w, req)

	if w.Code != http.StatusNotFound {
		t.Fatalf("deployment-id-shaped host: status = %d, want 404 from user app", w.Code)
	}
}

func TestTokenVerification(t *testing.T) {
	handler := NewEndpointHandler("demo", "test1234", "v1.0", "secret-token", http.NotFoundHandler())

	// Valid token → 200 + token echoed back
	req := httptest.NewRequest(http.MethodGet, "/status", nil)
	req.Host = "demo.tako"
	req.Header.Set("x-tako-internal-token", "secret-token")
	w := httptest.NewRecorder()
	handler.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("valid token: status = %d, want 200", w.Code)
	}
	if got := w.Header().Get("x-tako-internal-token"); got != "secret-token" {
		t.Errorf("response token = %q, want %q", got, "secret-token")
	}

	// Wrong token → 401
	req2 := httptest.NewRequest(http.MethodGet, "/status", nil)
	req2.Host = "demo.tako"
	req2.Header.Set("x-tako-internal-token", "wrong")
	w2 := httptest.NewRecorder()
	handler.ServeHTTP(w2, req2)

	if w2.Code != http.StatusUnauthorized {
		t.Errorf("wrong token: status = %d, want 401", w2.Code)
	}

	// Missing token → 401
	req3 := httptest.NewRequest(http.MethodGet, "/status", nil)
	req3.Host = "demo.tako"
	w3 := httptest.NewRecorder()
	handler.ServeHTTP(w3, req3)

	if w3.Code != http.StatusUnauthorized {
		t.Errorf("missing token: status = %d, want 401", w3.Code)
	}
}

func TestNoTokenInDevMode(t *testing.T) {
	// Empty token (dev mode) → no auth required
	handler := NewEndpointHandler("demo", "test1234", "v1.0", "", http.NotFoundHandler())

	req := httptest.NewRequest(http.MethodGet, "/status", nil)
	req.Host = "demo.tako"
	w := httptest.NewRecorder()
	handler.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Errorf("dev mode (no token): status = %d, want 200", w.Code)
	}
}

func TestDifferentAppTakoHostPassthrough(t *testing.T) {
	called := false
	userApp := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		called = true
		w.Write([]byte("user response"))
	})
	handler := NewEndpointHandler("demo", "test1234", "v1.0", "secret-token", userApp)

	req := httptest.NewRequest(http.MethodGet, "/status", nil)
	req.Host = "other.tako"
	req.Header.Set("x-tako-internal-token", "secret-token")
	w := httptest.NewRecorder()
	handler.ServeHTTP(w, req)

	if !called {
		t.Fatal("user app should be called for a different app-scoped .tako host")
	}
}

func TestChannelAuthorizeEndpoint(t *testing.T) {
	called := false
	handler := NewEndpointHandler("demo", "test1234", "v1.0", "secret-token", http.NotFoundHandler())
	Channels.Clear()
	defer Channels.Clear()

	Channels.Register("chat", ChannelDefinition{
		Auth: &ChannelAuthScheme{HeaderName: "authorization"},
		Verify: func(input VerifyInput) ChannelAuthDecision {
			called = true
			if input.Header == nil || input.Header.Scheme != "Bearer" || input.Header.Value != "test" {
				t.Fatalf("header = %#v, want Bearer test", input.Header)
			}
			if input.Channel != "chat" {
				t.Fatalf("channel = %q, want %q", input.Channel, "chat")
			}
			if input.Operation != ChannelOperationSubscribe {
				t.Fatalf("operation = %q, want %q", input.Operation, ChannelOperationSubscribe)
			}
			var params map[string]string
			if err := json.Unmarshal(input.Params, &params); err != nil {
				t.Fatal(err)
			}
			if params["roomId"] != "room-123" {
				t.Fatalf("roomId = %q, want room-123", params["roomId"])
			}
			return AllowChannel(ChannelGrant{
				Subject: "user-123",
				ChannelLifecycleConfig: ChannelLifecycleConfig{
					ReplayWindowMs:          86_400_000,
					InactivityTtlMs:         0,
					KeepaliveIntervalMs:     25_000,
					MaxConnectionLifetimeMs: 7_200_000,
				},
			})
		},
	})

	req := httptest.NewRequest(http.MethodPost, "/channels/authorize", strings.NewReader(`{
		"channel":"chat",
		"operation":"subscribe",
		"params":{"roomId":"room-123"},
		"header":{"scheme":"Bearer","value":"test"}
	}`))
	req.Host = "demo.tako"
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("x-tako-internal-token", "secret-token")
	w := httptest.NewRecorder()

	handler.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("status code = %d, want 200", w.Code)
	}
	if !called {
		t.Fatal("channel auth callback should be called")
	}

	var resp struct {
		OK                      bool   `json:"ok"`
		Subject                 string `json:"subject"`
		ReplayWindowMs          int64  `json:"replayWindowMs"`
		InactivityTtlMs         int64  `json:"inactivityTtlMs"`
		KeepaliveIntervalMs     int64  `json:"keepaliveIntervalMs"`
		MaxConnectionLifetimeMs int64  `json:"maxConnectionLifetimeMs"`
	}
	if err := json.NewDecoder(w.Body).Decode(&resp); err != nil {
		t.Fatalf("decode response: %v", err)
	}
	if !resp.OK {
		t.Fatal("expected ok response")
	}
	if resp.Subject != "user-123" {
		t.Fatalf("subject = %q, want %q", resp.Subject, "user-123")
	}
	if resp.ReplayWindowMs != 86_400_000 {
		t.Fatalf("replayWindowMs = %d, want %d", resp.ReplayWindowMs, 86_400_000)
	}
}

func TestChannelAuthorizeEndpointRejectsVerifyDenial(t *testing.T) {
	handler := NewEndpointHandler("demo", "test1234", "v1.0", "secret-token", http.NotFoundHandler())
	Channels.Clear()
	defer Channels.Clear()

	Channels.Register("chat", ChannelDefinition{
		Auth:   &ChannelAuthScheme{HeaderName: "authorization"},
		Verify: func(VerifyInput) ChannelAuthDecision { return RejectChannel() },
	})

	req := httptest.NewRequest(http.MethodPost, "/channels/authorize", strings.NewReader(`{
		"channel":"chat",
		"operation":"subscribe",
		"params":{},
		"header":{"scheme":"Bearer","value":"test"}
	}`))
	req.Host = "demo.tako"
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("x-tako-internal-token", "secret-token")
	w := httptest.NewRecorder()

	handler.ServeHTTP(w, req)

	if w.Code != http.StatusForbidden {
		t.Fatalf("status code = %d, want 403", w.Code)
	}
}

func TestChannelRegistryEndpoint(t *testing.T) {
	handler := NewEndpointHandler("demo", "test1234", "v1.0", "secret-token", http.NotFoundHandler())
	Channels.Clear()
	defer Channels.Clear()

	Channels.Register("chat", ChannelDefinition{
		ParamsSchema: []byte(`{"type":"object","properties":{"roomId":{"type":"string"}},"required":["roomId"]}`),
		Auth:         &ChannelAuthScheme{HeaderName: "authorization"},
		Transport:    ChannelTransportWS,
	})

	req := httptest.NewRequest(http.MethodGet, "/channels/registry", nil)
	req.Host = "demo.tako"
	req.Header.Set("x-tako-internal-token", "secret-token")
	w := httptest.NewRecorder()

	handler.ServeHTTP(w, req)

	if w.Code != http.StatusOK {
		t.Fatalf("status code = %d, want 200", w.Code)
	}
	var resp []struct {
		Channel      string          `json:"channel"`
		ParamsSchema json.RawMessage `json:"paramsSchema"`
		Auth         struct {
			HeaderName string `json:"headerName"`
		} `json:"auth"`
		Transport string `json:"transport"`
	}
	if err := json.NewDecoder(w.Body).Decode(&resp); err != nil {
		t.Fatalf("decode response: %v", err)
	}
	if len(resp) != 1 || resp[0].Channel != "chat" || resp[0].Auth.HeaderName != "authorization" || resp[0].Transport != "ws" {
		t.Fatalf("unexpected registry response: %#v", resp)
	}
}

func TestNonTakoHostPassthrough(t *testing.T) {
	called := false
	userApp := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		called = true
		w.Write([]byte("user response"))
	})

	handler := NewEndpointHandler("demo", "test1234", "v1.0", "", userApp)

	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.Host = "example.com"
	w := httptest.NewRecorder()
	handler.ServeHTTP(w, req)

	if !called {
		t.Error("user app should be called for non-tako host")
	}
}
