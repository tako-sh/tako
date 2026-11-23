package tako

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"sort"
	"time"

	"tako.sh/internal"
)

const (
	// kvMaxKeyBytes is the longest key.
	kvMaxKeyBytes = 1024
	// kvMaxValueBytes is the largest value, as JSON text.
	kvMaxValueBytes = 256 * 1024
	// kvMaxLineBytes bounds one request or reply line.
	kvMaxLineBytes = 1024 * 1024
)

// KV is a Tako-hosted KV store bound to this app in tako.toml
// (`[envs.<env>].kv`). It stores JSON values, each with an optional TTL.
// Keys without a TTL stay until deleted.
//
//	sessions, ok := tako.GetKV("sessions")
//	if !ok {
//		log.Fatal("no KV store bound as sessions")
//	}
//	err := sessions.Put(ctx, "session:42", Session{User: "u1"}, tako.WithTTL(24*time.Hour))
//	var session Session
//	found, err := sessions.Get(ctx, "session:42", &session)
//	err = sessions.Delete(ctx, "session:42")
//
// Requests go through the app's internal socket with the binding's token,
// in development and in production; the store may live on another server of
// the network.
type KV struct {
	name  string
	token string
}

// Name is the binding name from tako.toml.
func (k *KV) Name() string { return k.name }

// String names the binding without its token.
func (k *KV) String() string { return fmt.Sprintf("tako.KV(%s)", k.name) }

// GoString names the binding without its token for %#v as well.
func (k *KV) GoString() string { return k.String() }

// MarshalJSON names the binding without its token.
func (k *KV) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		KV string `json:"kv"`
	}{KV: k.name})
}

// KVOption configures a Put.
type KVOption func(*kvPut)

type kvPut struct {
	ttl    time.Duration
	hasTTL bool
}

// WithTTL makes the key expire after ttl, which must be positive. Putting
// the key again replaces its TTL; without WithTTL it stays until deleted.
func WithTTL(ttl time.Duration) KVOption {
	return func(put *kvPut) { put.ttl, put.hasTTL = ttl, true }
}

// Get decodes the key's JSON value into value and reports whether the key
// was set. A key that is not set or has expired leaves value unchanged.
func (k *KV) Get(ctx context.Context, key string, value any) (bool, error) {
	data, err := k.call(ctx, key, map[string]any{"operation": "get"})
	if err != nil {
		return false, err
	}
	var text *string
	if err := json.Unmarshal(data, &text); err != nil {
		return false, errors.New("tako: invalid KV response")
	}
	if text == nil {
		return false, nil
	}
	if err := json.Unmarshal([]byte(*text), value); err != nil {
		return true, fmt.Errorf("tako: decode KV value %q: %w", key, err)
	}
	return true, nil
}

// Put sets the key to value, encoded as JSON.
func (k *KV) Put(ctx context.Context, key string, value any, options ...KVOption) error {
	var put kvPut
	for _, option := range options {
		option(&put)
	}
	encoded, err := json.Marshal(value)
	if err != nil {
		return fmt.Errorf("tako: encode KV value: %w", err)
	}
	if len(encoded) > kvMaxValueBytes {
		return errors.New("tako: KV value exceeds 256 KiB")
	}
	request := map[string]any{"operation": "put", "json": string(encoded)}
	if put.hasTTL {
		// A zero TTL would otherwise mean no expiry, as from a deadline
		// that has just passed.
		if put.ttl <= 0 {
			return errors.New("tako: KV ttl must be positive")
		}
		// Round up so a sub-millisecond TTL still expires rather than failing.
		request["ttl_ms"] = (put.ttl + time.Millisecond - 1).Milliseconds()
	}
	_, err = k.call(ctx, key, request)
	return err
}

// Delete removes the key. Removing a key that is not set succeeds.
func (k *KV) Delete(ctx context.Context, key string) error {
	_, err := k.call(ctx, key, map[string]any{"operation": "delete"})
	return err
}

func (k *KV) call(ctx context.Context, key string, request map[string]any) (data json.RawMessage, err error) {
	if key == "" {
		return nil, errors.New("tako: KV key cannot be empty")
	}
	if len(key) > kvMaxKeyBytes {
		return nil, errors.New("tako: KV key exceeds 1 KiB")
	}
	socket, app := os.Getenv("TAKO_INTERNAL_SOCKET"), os.Getenv("TAKO_APP_NAME")
	if socket == "" || app == "" {
		if err := internal.ComposeSDKError(false); err != nil {
			return nil, err
		}
		return nil, errors.New("tako: KV stores require the Tako runtime")
	}
	request["command"] = "kv"
	request["app"] = app
	request["token"] = k.token
	request["key"] = key
	body, err := json.Marshal(request)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	defer func() {
		// Cancellation closes the connection to interrupt in-flight I/O. Report
		// the context error rather than the socket error that causes.
		if err != nil && ctx.Err() != nil {
			err = ctx.Err()
		}
	}()
	conn, err := (&net.Dialer{}).DialContext(ctx, "unix", socket)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	stop := context.AfterFunc(ctx, func() { _ = conn.Close() })
	defer stop()
	if _, err := io.Copy(conn, bytes.NewReader(append(body, '\n'))); err != nil {
		return nil, err
	}
	line, err := bufio.NewReader(io.LimitReader(conn, kvMaxLineBytes+1)).ReadBytes('\n')
	if err != nil {
		return nil, err
	}
	if len(line) > kvMaxLineBytes {
		return nil, errors.New("tako: KV response too large")
	}
	var response struct {
		Status  string          `json:"status"`
		Data    json.RawMessage `json:"data"`
		Code    string          `json:"code"`
		Message string          `json:"message"`
	}
	if err := json.Unmarshal(line, &response); err != nil {
		return nil, errors.New("tako: invalid KV response")
	}
	if response.Status != "ok" {
		return nil, &ServerError{Code: response.Code, Message: response.Message}
	}
	if len(response.Data) == 0 {
		return json.RawMessage("null"), nil
	}
	return response.Data, nil
}

// GetKV returns the KV store bound to this app as name.
func GetKV(name string) (*KV, bool) {
	if bootstrap == nil {
		return nil, false
	}
	connection, ok := bootstrap.KV[name]
	if !ok || connection.Token == "" {
		return nil, false
	}
	return &KV{name: name, token: connection.Token}, true
}

// KVNames returns the names of this app's KV store bindings, sorted.
func KVNames() []string {
	if bootstrap == nil {
		return nil
	}
	names := make([]string, 0, len(bootstrap.KV))
	for name := range bootstrap.KV {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}
