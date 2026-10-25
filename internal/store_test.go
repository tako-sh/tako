package internal

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"math"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func clearStoresForTest(t *testing.T) {
	t.Helper()
	clear := func() {
		stores.Lock()
		stores.definitions = make(map[string]StoreDefinition)
		stores.Unlock()
		Channels.Clear()
	}
	clear()
	t.Cleanup(clear)
}

func storeRequest(handler http.Handler, method, host, token, body string) *httptest.ResponseRecorder {
	request := httptest.NewRequest(method, internalStoreLoadPath, strings.NewReader(body))
	request.Host = host
	request.Header.Set(internalTokenHeader, token)
	recorder := httptest.NewRecorder()
	handler.ServeHTTP(recorder, request)
	return recorder
}

func TestStoreLoadRequiresInternalHostAndTokenBeforeCallingBackend(t *testing.T) {
	clearStoresForTest(t)
	calls := 0
	RegisterStore("private", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) {
		calls++
		return "secret-data", nil
	}})
	for _, tc := range []struct {
		name, host, configuredToken, requestToken string
		want                                      int
	}{
		{"public host", "demo.example", "secret", "secret", 404},
		{"another app", "other.tako", "secret", "secret", 404},
		{"missing token", "demo.tako", "secret", "", 401},
		{"wrong token", "demo.tako", "secret", "wrong", 401},
		{"no configured token", "demo.tako", "", "", 403},
	} {
		t.Run(tc.name, func(t *testing.T) {
			handler := NewEndpointHandler("demo/production", "instance", "version", tc.configuredToken, http.NotFoundHandler())
			response := storeRequest(handler, http.MethodPost, tc.host, tc.requestToken, `{"store":"private","params":{},"subject":"forged-user"}`)
			if response.Code != tc.want {
				t.Fatalf("status %d, want %d: %s", response.Code, tc.want, response.Body)
			}
			if strings.Contains(response.Body.String(), "secret-data") {
				t.Fatal("unauthorized request exposed backend data")
			}
		})
	}
	if calls != 0 {
		t.Fatalf("unauthorized requests called backend %d times", calls)
	}
}

func TestStoreLoadPassesTrustedContextAndOfflinePolicy(t *testing.T) {
	clearStoresForTest(t)
	type contextKey struct{}
	offline := &StoreOfflineConfig{MaxAgeMs: 60_000}
	schema := json.RawMessage(`{"type":"object","properties":{"chatId":{"type":"string"}},"required":["chatId"]}`)
	handle := RegisterStore("messages", StoreDefinition{
		ParamsSchema:            schema,
		Offline:                 offline,
		Auth:                    &ChannelAuthScheme{CookieName: "session"},
		MaxConnectionLifetimeMs: 30_000,
		Verify: func(input VerifyInput) ChannelAuthDecision {
			if input.Channel != "messages" || input.Operation != ChannelOperationSubscribe || input.Cookie == nil || *input.Cookie != "valid" {
				return RejectChannel()
			}
			return AllowChannel(ChannelGrant{Subject: "verified-user"})
		},
		Load: func(ctx context.Context, input StoreLoadInput) (any, error) {
			if ctx.Value(contextKey{}) != "request-context" || input.Store != "messages" || input.Subject != "verified-user" || string(input.Params) != `{"chatId":"one"}` {
				t.Fatalf("unexpected load context: %#v, %v", input, ctx.Value(contextKey{}))
			}
			// Application code can register another store during a load. The
			// registry must not hold its lock while invoking this function.
			RegisterStore("nested", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil }})
			return []map[string]string{{"id": "message-1"}}, nil
		},
	})
	if handle.Name() != "messages" {
		t.Fatalf("unexpected name %q", handle.Name())
	}
	// Registration owns its policy, rather than retaining mutable config.
	offline.MaxAgeMs = 1
	offline.Version = "mutated"
	cookie := "valid"
	grant, defined, allowed := Channels.Authorize(ChannelAuthorizeInput{
		Channel: storeChannelPrefix + "messages", Operation: ChannelOperationSubscribe,
		Params: json.RawMessage(`{"chatId":"one"}`), Cookie: &cookie,
	})
	if !defined || !allowed || grant.Subject != "verified-user" || grant.MaxConnectionLifetimeMs != 30_000 || grant.Transport != "" {
		t.Fatalf("unexpected store authorization: %#v", grant)
	}
	handler := NewEndpointHandler("demo", "instance", "version", "secret", http.NotFoundHandler())
	request := httptest.NewRequest(http.MethodPost, internalStoreLoadPath, strings.NewReader(`{"store":"messages","params":{"chatId":"one"},"subject":"verified-user"}`))
	request.Host = "demo.tako"
	request.Header.Set(internalTokenHeader, "secret")
	request = request.WithContext(context.WithValue(request.Context(), contextKey{}, "request-context"))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != 200 || response.Header().Get("Cache-Control") != "no-store" || response.Header().Get(internalTokenHeader) != "secret" {
		t.Fatalf("unexpected response: %d %v %s", response.Code, response.Header(), response.Body)
	}
	var body struct {
		Data    []map[string]string `json:"data"`
		Offline StoreOfflineConfig  `json:"offline"`
	}
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	if len(body.Data) != 1 || body.Data[0]["id"] != "message-1" || body.Offline.MaxAgeMs != 60_000 || body.Offline.Version != "1" {
		t.Fatalf("unexpected data/offline policy: %#v", body)
	}
}

func TestStoreLoadRejectsMalformedAndOversizedRequests(t *testing.T) {
	clearStoresForTest(t)
	calls := 0
	RegisterStore("data", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { calls++; return nil, nil }})
	handler := NewEndpointHandler("demo", "instance", "version", "secret", http.NotFoundHandler())
	for _, body := range []string{
		``, `null`, `[]`, `{"store":"data"}`, `{"store":"data","params":null}`,
		`{"store":"data","params":[]}`, `{"store":"data","params":1}`,
		`{"store":"data","params":{},"header":{"value":"forged"}}`,
		`{"store":"data","params":{}} {}`, `{"store":"data","params":{}} garbage`,
		`{"store":"data","params":{"value":"` + strings.Repeat("x", maxInternalBodyBytes) + `"}}`,
		`{"store":"data","params":{}}` + strings.Repeat(" ", maxInternalBodyBytes),
	} {
		response := storeRequest(handler, http.MethodPost, "demo.tako", "secret", body)
		if response.Code != 400 {
			t.Fatalf("malformed request status %d for %.80q: %s", response.Code, body, response.Body)
		}
	}
	if calls != 0 {
		t.Fatalf("malformed requests called backend %d times", calls)
	}
	if response := storeRequest(handler, http.MethodGet, "demo.tako", "secret", ""); response.Code != 405 || response.Header().Get("Allow") != "POST" {
		t.Fatalf("unexpected method response: %d %v", response.Code, response.Header())
	}
	if response := storeRequest(handler, http.MethodPost, "demo.tako", "secret", `{"store":"missing","params":{}}`); response.Code != 404 {
		t.Fatalf("missing store status %d", response.Code)
	}
}

func TestStoreLoadNeverExposesBackendErrorsOrInvalidJSON(t *testing.T) {
	clearStoresForTest(t)
	handler := NewEndpointHandler("demo", "instance", "version", "secret", http.NotFoundHandler())
	for _, tc := range []struct {
		name string
		data any
		err  error
	}{
		{"backend error", nil, errors.New("database-password=private-secret")},
		{"unserializable", math.NaN(), nil},
		{"oversized", strings.Repeat("x", maxInternalBodyBytes), nil},
		{"bad raw JSON", json.RawMessage(`{"private-secret":`), nil},
	} {
		t.Run(tc.name, func(t *testing.T) {
			RegisterStore("data", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return tc.data, tc.err }})
			response := storeRequest(handler, http.MethodPost, "demo.tako", "secret", `{"store":"data","params":{}}`)
			if response.Code != 500 || response.Body.String() != `{"error":"Store load failed"}` {
				t.Fatalf("error exposed private details: %d %.100s", response.Code, response.Body)
			}
		})
	}
}

func TestStoreSubscriptionsAreReadOnlyIncludingPublicStores(t *testing.T) {
	clearStoresForTest(t)
	RegisterStore("public", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil }})
	handler := NewEndpointHandler("demo", "instance", "version", "secret", http.NotFoundHandler())
	for _, operation := range []string{"publish", "connect", "unknown"} {
		request := httptest.NewRequest(http.MethodPost, internalChannelAuthorizePath, strings.NewReader(`{"channel":"tako.store:public","operation":"`+operation+`","params":{}}`))
		request.Host = "demo.tako"
		request.Header.Set(internalTokenHeader, "secret")
		response := httptest.NewRecorder()
		handler.ServeHTTP(response, request)
		if response.Code != 403 {
			t.Fatalf("%s authorization returned %d", operation, response.Code)
		}
	}
}

func TestRegisterStoreRejectsInvalidConfiguration(t *testing.T) {
	clearStoresForTest(t)
	load := func(context.Context, StoreLoadInput) (any, error) { return nil, nil }
	for _, tc := range []struct {
		name string
		def  StoreDefinition
	}{
		{"", StoreDefinition{Load: load}},
		{"/bad", StoreDefinition{Load: load}},
		{"bad/name", StoreDefinition{Load: load}},
		{"has spaces", StoreDefinition{Load: load}},
		{"_bad", StoreDefinition{Load: load}},
		{"bad\x01name", StoreDefinition{Load: load}},
		{"非ASCII", StoreDefinition{Load: load}},
		{strings.Repeat("a", 257), StoreDefinition{Load: load}},
		{"data", StoreDefinition{}},
		{"data", StoreDefinition{Load: load, ParamsSchema: json.RawMessage(`null`)}},
		{"data", StoreDefinition{Load: load, Offline: &StoreOfflineConfig{MaxAgeMs: 0}}},
		{"data", StoreDefinition{Load: load, Offline: &StoreOfflineConfig{MaxAgeMs: -1}}},
		{"data", StoreDefinition{Load: load, Offline: &StoreOfflineConfig{MaxAgeMs: 9_007_199_254_740_992}}},
		{"data", StoreDefinition{Load: load, Offline: &StoreOfflineConfig{MaxAgeMs: 1, Version: strings.Repeat("v", 129)}}},
		{"data", StoreDefinition{Load: load, MaxConnectionLifetimeMs: -1}},
		{"data", StoreDefinition{Load: load, KeepaliveIntervalMs: 9_007_199_254_740_992}},
		{"data", StoreDefinition{Load: load, MaxConnectionLifetimeMs: 9_007_199_254_740_992}},
	} {
		func() {
			defer func() {
				if recover() == nil {
					t.Errorf("RegisterStore accepted invalid config: %#v", tc)
				}
			}()
			RegisterStore(tc.name, tc.def)
		}()
	}
}

func storeSocket(t *testing.T, serve func(net.Conn)) {
	t.Helper()
	dir, err := os.MkdirTemp("", "tako-store-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	path := filepath.Join(dir, "rpc.sock")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	t.Setenv("TAKO_INTERNAL_SOCKET", path)
	t.Setenv("TAKO_APP_NAME", "demo/production")
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		serve(conn)
	}()
}

func TestStoreInvalidationUsesScopedTrustedSocketCommand(t *testing.T) {
	clearStoresForTest(t)
	handle := RegisterStore("messages", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil }})
	commands := make(chan map[string]json.RawMessage, 1)
	storeSocket(t, func(conn net.Conn) {
		var command map[string]json.RawMessage
		if err := json.NewDecoder(conn).Decode(&command); err != nil {
			commands <- nil
			return
		}
		commands <- command
		_, _ = io.WriteString(conn, "{\"status\":\"ok\",\"data\":{\"id\":\"1\"}}\n")
	})
	if err := handle.Invalidate(context.Background(), map[string]any{"chatId": "private", "page": 2}); err != nil {
		t.Fatal(err)
	}
	command := <-commands
	if string(command["command"]) != `"channel_publish"` || string(command["app"]) != `"demo/production"` || string(command["channel"]) != `"tako.store:messages"` {
		t.Fatalf("unexpected socket command: %s", mustJSON(t, command))
	}
	var payload struct {
		Type        string                 `json:"type"`
		Data        map[string]any         `json:"data"`
		ScopeParams map[string]any         `json:"scopeParams"`
		Lifecycle   ChannelLifecycleConfig `json:"lifecycle"`
	}
	if err := json.Unmarshal(command["payload"], &payload); err != nil {
		t.Fatal(err)
	}
	if payload.Type != "tako.store.invalidate" || payload.Data == nil || len(payload.Data) != 0 || payload.ScopeParams["chatId"] != "private" || payload.ScopeParams["page"] != float64(2) || payload.Lifecycle.MaxConnectionLifetimeMs <= 0 {
		t.Fatalf("unexpected publication payload: %#v", payload)
	}
}

func mustJSON(t *testing.T, value any) string {
	t.Helper()
	body, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return string(body)
}

func TestStoreInvalidationHonorsCancellationAfterSocketConnected(t *testing.T) {
	clearStoresForTest(t)
	handle := RegisterStore("messages", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil }})
	connected := make(chan struct{})
	storeSocket(t, func(conn net.Conn) {
		_, _ = bufio.NewReader(conn).ReadBytes('\n')
		close(connected)
		_, _ = io.Copy(io.Discard, conn)
	})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	result := make(chan error, 1)
	go func() { result <- handle.Invalidate(ctx, nil) }()
	select {
	case <-connected:
	case <-time.After(2 * time.Second):
		t.Fatal("socket did not receive invalidation")
	}
	cancel()
	select {
	case err := <-result:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("error = %v, want context.Canceled", err)
		}
	case <-time.After(time.Second):
		t.Fatal("invalidation did not stop after cancellation")
	}
}

func TestStoreInvalidationRejectsInvalidInputsBeforeSocketIO(t *testing.T) {
	clearStoresForTest(t)
	handle := RegisterStore("messages", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil }})
	t.Setenv("TAKO_INTERNAL_SOCKET", "")
	t.Setenv("TAKO_APP_NAME", "")
	for _, params := range []any{[]string{"not-object"}, 1, json.RawMessage(`null`), map[string]any{"invalid": math.NaN()}} {
		if err := handle.Invalidate(context.Background(), params); err == nil || !strings.Contains(err.Error(), "JSON object") {
			t.Fatalf("params %#v accepted: %v", params, err)
		}
	}
	if err := handle.Invalidate(context.Background(), nil); err == nil || !strings.Contains(err.Error(), "TAKO_INTERNAL_SOCKET") {
		t.Fatalf("missing runtime accepted: %v", err)
	}
	if err := handle.Invalidate(context.Background(), map[string]string{"tooLarge": strings.Repeat("x", maxStoreParamsBytes)}); err == nil || !strings.Contains(err.Error(), "16 KiB") {
		t.Fatalf("oversized params accepted: %v", err)
	}
}

func TestStoreInvalidationBoundsAndSanitizesSocketResponses(t *testing.T) {
	clearStoresForTest(t)
	handle := RegisterStore("messages", StoreDefinition{Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil }})
	for _, response := range []string{
		"{\"status\":\"error\",\"message\":\"database-password=private-secret\"}\n",
		"{\"status\":\"unknown\"}\n", "not-json\n", "",
		strings.Repeat("x", maxInternalBodyBytes+1) + "\n",
	} {
		storeSocket(t, func(conn net.Conn) {
			_, _ = bufio.NewReader(conn).ReadBytes('\n')
			_, _ = io.WriteString(conn, response)
		})
		err := handle.Invalidate(context.Background(), nil)
		if err == nil || strings.Contains(err.Error(), "private-secret") {
			t.Fatalf("unexpected socket error: %v", err)
		}
	}
}
