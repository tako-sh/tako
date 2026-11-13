package tako

// Pins the network and store requests and reply parsing to the wire fixtures
// tako-core owns in test-support/wire/ (see its README).

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"
)

const wireFixtureDir = "test-support/wire"

func wireFixture(t *testing.T, path string) json.RawMessage {
	t.Helper()
	if _, err := os.Stat(wireFixtureDir); err != nil {
		t.Fatalf("wire fixtures not found at %s (run from a full checkout): %v", wireFixtureDir, err)
	}
	data, err := os.ReadFile(filepath.Join(wireFixtureDir, path))
	if err != nil {
		t.Fatalf("read wire fixture: %v", err)
	}
	return data
}

func decodeWire(t *testing.T, data []byte) map[string]any {
	t.Helper()
	var value map[string]any
	if err := json.Unmarshal(data, &value); err != nil {
		t.Fatalf("decode %s: %v", data, err)
	}
	return value
}

// wireSocket records each command and answers with queued reply fixtures
// (replies/ok.json when none is queued). It sets the runtime env the network
// and store clients read.
type wireSocket struct {
	t        *testing.T
	mu       sync.Mutex
	received [][]byte
	replies  []json.RawMessage
}

func startWireSocket(t *testing.T) *wireSocket {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "tako-wire-")
	if err != nil {
		t.Fatalf("tempdir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	path := filepath.Join(dir, "internal.sock")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	t.Setenv("TAKO_INTERNAL_SOCKET", path)
	t.Setenv("TAKO_APP_NAME", "demo")
	s := &wireSocket{t: t}
	ok := wireFixture(t, "replies/ok.json")
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			line, err := bufio.NewReader(conn).ReadBytes('\n')
			if err == nil {
				s.mu.Lock()
				s.received = append(s.received, line)
				reply := ok
				if len(s.replies) > 0 {
					reply, s.replies = s.replies[0], s.replies[1:]
				}
				s.mu.Unlock()
				var compact strings.Builder
				_ = json.NewEncoder(&compact).Encode(json.RawMessage(reply))
				_, _ = conn.Write([]byte(compact.String()))
			}
			_ = conn.Close()
		}
	}()
	return s
}

func (s *wireSocket) reply(path string) {
	data := wireFixture(s.t, path)
	s.mu.Lock()
	defer s.mu.Unlock()
	s.replies = append(s.replies, data)
}

// take returns the one command sent since the last call.
func (s *wireSocket) take() map[string]any {
	s.t.Helper()
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.received) != 1 {
		s.t.Fatalf("expected one command, got %d", len(s.received))
	}
	line := s.received[0]
	s.received = nil
	return decodeWire(s.t, line)
}

func (s *wireSocket) expect(fixture string, got map[string]any) {
	s.t.Helper()
	want := decodeWire(s.t, wireFixture(s.t, "commands/"+fixture+".json"))
	if !reflect.DeepEqual(got, want) {
		s.t.Errorf("%s:\n  sent:    %v\n  fixture: %v", fixture, got, want)
	}
}

func TestNetworkCommandsMatchWireFixtures(t *testing.T) {
	socket := startWireSocket(t)
	ctx := context.Background()

	if _, err := Call(ctx, "users.lookup", map[string]any{"id": "u1"}); err != nil {
		t.Fatal(err)
	}
	socket.expect("network_call-call", socket.take())

	socket.reply("replies/run_result-succeeded.json")
	result, err := Result(ctx, "mail.send-email", "run-1")
	if err != nil || !reflect.DeepEqual(decodeWire(t, result), map[string]any{"sent": true}) {
		t.Fatalf("run_result-succeeded parsed as %s, %v", result, err)
	}
	socket.expect("network_call-result", socket.take())

	runAt := int64(1_767_225_600_000)
	maxAttempts := uint32(5)
	id := "order-42"
	socket.reply("replies/enqueue_run.json")
	enqueued, err := Enqueue(ctx, "mail.send-email", map[string]any{"to": "a@example.com"}, NetworkEnqueueOptions{
		RunAtMs: &runAt, MaxAttempts: &maxAttempts, ID: &id,
	})
	if err != nil || *enqueued != (NetworkEnqueueResult{ID: "order-42", Deduplicated: true}) {
		t.Fatalf("enqueue_run parsed as %+v, %v", enqueued, err)
	}
	socket.expect("network_call-enqueue", socket.take())

	if _, err := Publish(ctx, "chat.messages", map[string]any{"room": "lobby"}, "message", map[string]any{"text": "hi"}); err != nil {
		t.Fatal(err)
	}
	socket.expect("network_call-publish", socket.take())
}

func TestNetworkResultRepliesFromWireFixtures(t *testing.T) {
	socket := startWireSocket(t)
	ctx := context.Background()

	socket.reply("replies/run_result-dead.json")
	_, err := Result(ctx, "mail.send-email", "run-1")
	var failed *WorkflowResultError
	if !errors.As(err, &failed) || failed.Status != "dead" || failed.Message != "smtp timeout" {
		t.Fatalf("run_result-dead parsed as %#v", err)
	}
	socket.take()

	socket.reply("replies/ok-null.json")
	_, err = Result(ctx, "mail.send-email", "run-1")
	if !errors.As(err, &failed) || failed.Status != "missing" {
		t.Fatalf("a null result must mean no retained run: %#v", err)
	}
	socket.take()
}

func TestStoreInvalidationMatchesWireFixture(t *testing.T) {
	Channels.Clear()
	defer Channels.Clear()
	socket := startWireSocket(t)
	store := RegisterStore("messages", StoreDefinition{
		ParamsSchema: json.RawMessage(`{"type":"object"}`),
		Load:         func(context.Context, StoreLoadInput) (any, error) { return nil, nil },
	})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := store.Invalidate(ctx, map[string]any{"chatId": "chat-a"}); err != nil {
		t.Fatal(err)
	}
	got := socket.take()
	// Known gap: the Go SDK adds the store's channel lifecycle to the payload;
	// the JS and Rust SDKs send none, and the server accepts both. Which one
	// store invalidations should carry is unsettled, so the fixture describes
	// the payload without it and this check drops it.
	payload, _ := got["payload"].(map[string]any)
	if _, ok := payload["lifecycle"]; !ok {
		t.Fatalf("known gap closed: the Go store invalidation no longer sends a lifecycle; remove this exception")
	}
	delete(payload, "lifecycle")
	socket.expect("channel_publish-store_invalidate", got)
}

func TestErrorRepliesFromWireFixturesMapToSentinels(t *testing.T) {
	sentinels := map[string]error{
		"error.json":                        nil,
		"error-unknown_code.json":           nil,
		"error-run_not_owned.json":          ErrRunNotOwned,
		"error-run_id_conflict.json":        ErrRunIDConflict,
		"error-invalid_request.json":        ErrInvalidRequest,
		"error-channel_unknown.json":        ErrChannelUnknown,
		"error-channel_params_invalid.json": ErrChannelParamsInvalid,
		"error-channel_closed.json":         ErrChannelClosed,
		"error-unavailable.json":            ErrUnavailable,
	}
	all := []error{
		ErrRunNotOwned, ErrRunIDConflict, ErrInvalidRequest, ErrChannelUnknown,
		ErrChannelParamsInvalid, ErrChannelClosed, ErrUnavailable,
	}
	entries, err := os.ReadDir(filepath.Join(wireFixtureDir, "replies"))
	if err != nil {
		t.Fatalf("wire fixtures not found at %s (run from a full checkout): %v", wireFixtureDir, err)
	}
	var names []string
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "error") {
			names = append(names, entry.Name())
		}
	}
	sort.Strings(names)
	if len(names) != len(sentinels) {
		t.Fatalf("error fixtures %v do not match the sentinels this test knows", names)
	}

	Channels.Clear()
	defer Channels.Clear()
	socket := startWireSocket(t)
	store := RegisterStore("messages", StoreDefinition{
		Load: func(context.Context, StoreLoadInput) (any, error) { return nil, nil },
	})
	for _, name := range names {
		sentinel, known := sentinels[name]
		if !known {
			t.Fatalf("no sentinel for %s", name)
		}
		code, _ := decodeWire(t, wireFixture(t, "replies/"+name))["code"].(string)
		// Network calls and store invalidations drop the server's diagnostic,
		// which may describe another app's internals; the code is kept.
		for _, send := range []func() error{
			func() error { _, err := Call(context.Background(), "users.lookup", nil); return err },
			func() error { return store.Invalidate(context.Background(), nil) },
		} {
			socket.reply("replies/" + name)
			err := send()
			var serverErr *ServerError
			if !errors.As(err, &serverErr) || serverErr.Code != code {
				t.Fatalf("%s: %#v", name, err)
			}
			for _, other := range all {
				if errors.Is(err, other) != (other == sentinel) {
					t.Fatalf("%s: errors.Is(%v) = %v", name, other, errors.Is(err, other))
				}
			}
			socket.take()
		}
	}
}
