package tako

// Pins the workflow client's internal-socket requests and reply parsing to
// the wire fixtures tako-core owns in test-support/wire/ (see its README).

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

// The SDK module cannot embed files outside its directory; read the checked-in
// fixtures by path instead.
const wireFixtureDir = "../../test-support/wire"

const (
	wireApp     = "demo"
	wireRunID   = "run-1"
	wireClaim   = "claim-7"
	wireAtMs    = int64(1_767_225_600_000) // 2026-01-01T00:00:00Z
	wireLeaseMs = uint64(30_000)
)

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

func decodeJSON(t *testing.T, data []byte) any {
	t.Helper()
	var value any
	if err := json.Unmarshal(data, &value); err != nil {
		t.Fatalf("decode %s: %v", data, err)
	}
	return value
}

// wireSocket records each command and answers with queued reply fixtures
// (replies/ok.json when none is queued).
type wireSocket struct {
	t        *testing.T
	path     string
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
	s := &wireSocket{t: t, path: filepath.Join(dir, "internal.sock")}
	listener, err := net.Listen("unix", s.path)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { _ = listener.Close() })
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
func (s *wireSocket) take() []byte {
	s.t.Helper()
	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.received) != 1 {
		s.t.Fatalf("expected one command, got %d", len(s.received))
	}
	line := s.received[0]
	s.received = nil
	return line
}

func ptr[T any](value T) *T { return &value }

func TestWorkflowCommandsMatchWireFixtures(t *testing.T) {
	socket := startWireSocket(t)
	client := NewClient(socket.path, wireApp)
	ctx := context.Background()
	at := func(offset int64) *time.Time { return ptr(time.UnixMilli(wireAtMs + offset)) }
	payload := map[string]any{"to": "a@example.com"}

	cases := []struct {
		fixture string
		reply   string
		send    func() error
	}{
		{"enqueue_run", "replies/enqueue_run.json", func() error {
			_, err := client.Enqueue(ctx, "send-email", payload, EnqueueOpts{
				RunAt: at(0), MaxAttempts: ptr(uint32(5)), ID: ptr("order-42"),
			})
			return err
		}},
		{"enqueue_run-defaults", "replies/enqueue_run.json", func() error {
			_, err := client.Enqueue(ctx, "send-email", payload, EnqueueOpts{})
			return err
		}},
		{"get_run_result", "replies/ok-null.json", func() error {
			_, err := client.Result(ctx, "send-email", "order-42")
			return err
		}},
		{"register_schedules", "", func() error {
			return client.RegisterSchedules(ctx, []ScheduleSpec{
				{Name: "nightly-report", Cron: "0 3 * * *", MaxAttempts: 5},
				{Name: "cleanup", Cron: "*/15 * * * *"},
			})
		}},
		{"register_schedules-empty", "", func() error {
			return client.RegisterSchedules(ctx, []ScheduleSpec{})
		}},
		// A worker that collects no schedules holds a nil slice.
		{"register_schedules-empty", "", func() error {
			return client.RegisterSchedules(ctx, nil)
		}},
		{"claim_run", "replies/ok-null.json", func() error {
			_, err := client.Claim(ctx, "worker-1", []string{"send-email", "nightly-report"}, wireLeaseMs)
			return err
		}},
		{"heartbeat_run", "", func() error { return client.Heartbeat(ctx, wireRunID, wireClaim, wireLeaseMs) }},
		{"save_step", "", func() error {
			return client.SaveStep(ctx, wireRunID, wireClaim, "fetch-user", map[string]any{"id": "u1"})
		}},
		{"complete_run", "", func() error {
			return client.Complete(ctx, wireRunID, wireClaim, map[string]any{"sent": true})
		}},
		{"cancel_run", "", func() error { return client.Cancel(ctx, wireRunID, wireClaim, ptr("customer asked")) }},
		{"cancel_run-no_reason", "", func() error { return client.Cancel(ctx, wireRunID, wireClaim, nil) }},
		{"fail_run-retry", "", func() error {
			return client.Fail(ctx, wireRunID, wireClaim, "smtp timeout", at(60_000), false)
		}},
		{"fail_run-finalize", "", func() error {
			return client.Fail(ctx, wireRunID, wireClaim, "smtp timeout", nil, true)
		}},
		{"defer_run", "", func() error { return client.Defer(ctx, wireRunID, wireClaim, at(3_600_000)) }},
		{"defer_run-indefinite", "", func() error { return client.Defer(ctx, wireRunID, wireClaim, nil) }},
		{"wait_for_event", "", func() error {
			return client.WaitForEvent(ctx, wireRunID, wireClaim, "approval", "order-approved", at(86_400_000))
		}},
		{"wait_for_event-no_timeout", "", func() error {
			return client.WaitForEvent(ctx, wireRunID, wireClaim, "approval", "order-approved", nil)
		}},
		{"signal", "replies/signal.json", func() error {
			_, err := client.Signal(ctx, "order-approved", map[string]any{"orderId": "42"})
			return err
		}},
	}
	for _, tc := range cases {
		if tc.reply != "" {
			socket.reply(tc.reply)
		}
		if err := tc.send(); err != nil {
			t.Fatalf("%s: %v", tc.fixture, err)
		}
		got := decodeJSON(t, socket.take())
		want := decodeJSON(t, wireFixture(t, "commands/"+tc.fixture+".json"))
		if !reflect.DeepEqual(got, want) {
			t.Errorf("%s:\n  sent:    %v\n  fixture: %v", tc.fixture, got, want)
		}
	}
}

func TestWorkflowRepliesParseFromWireFixtures(t *testing.T) {
	socket := startWireSocket(t)
	client := NewClient(socket.path, wireApp)
	ctx := context.Background()

	socket.reply("replies/claim_run.json")
	run, err := client.Claim(ctx, "worker-1", []string{"send-email"}, wireLeaseMs)
	if err != nil {
		t.Fatal(err)
	}
	wantRun := &Run{
		WorkerID:    wireClaim,
		ID:          wireRunID,
		Name:        "send-email",
		Status:      "running",
		Attempts:    2,
		MaxAttempts: 5,
		RunAtMs:     wireAtMs,
		StepState:   map[string]any{"fetch-user": map[string]any{"id": "u1"}},
	}
	gotPayload := decodeJSON(t, run.Payload)
	run.Payload = nil
	if !reflect.DeepEqual(run, wantRun) || !reflect.DeepEqual(gotPayload, map[string]any{"to": "a@example.com"}) {
		t.Fatalf("claim_run parsed as %+v with payload %v", run, gotPayload)
	}

	socket.reply("replies/ok-null.json")
	if run, err := client.Claim(ctx, "worker-1", []string{"send-email"}, wireLeaseMs); err != nil || run != nil {
		t.Fatalf("a null claim must mean nothing is due: %+v, %v", run, err)
	}

	socket.reply("replies/enqueue_run.json")
	enqueued, err := client.Enqueue(ctx, "send-email", nil, EnqueueOpts{})
	if err != nil || *enqueued != (EnqueueResult{ID: "order-42", Deduplicated: true}) {
		t.Fatalf("enqueue_run parsed as %+v, %v", enqueued, err)
	}

	socket.reply("replies/run_result-succeeded.json")
	result, err := client.Result(ctx, "send-email", wireRunID)
	if err != nil || result.Status != "succeeded" || result.Error != "" ||
		!reflect.DeepEqual(decodeJSON(t, result.Result), map[string]any{"sent": true}) {
		t.Fatalf("run_result-succeeded parsed as %+v, %v", result, err)
	}
	socket.reply("replies/run_result-dead.json")
	result, err = client.Result(ctx, "send-email", wireRunID)
	if err != nil || result.Status != "dead" || result.Error != "smtp timeout" || string(result.Result) != "null" {
		t.Fatalf("run_result-dead parsed as %+v, %v", result, err)
	}
	socket.reply("replies/ok-null.json")
	if result, err := client.Result(ctx, "send-email", wireRunID); err != nil || result != nil {
		t.Fatalf("a null result must mean no retained run: %+v, %v", result, err)
	}

	socket.reply("replies/signal.json")
	if woken, err := client.Signal(ctx, "order-approved", nil); err != nil || woken != 2 {
		t.Fatalf("signal parsed as %d, %v", woken, err)
	}

	socket.reply("replies/ok.json")
	if err := client.Heartbeat(ctx, wireRunID, wireClaim, wireLeaseMs); err != nil {
		t.Fatalf("ok reply: %v", err)
	}
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

	socket := startWireSocket(t)
	client := NewClient(socket.path, wireApp)
	for _, name := range names {
		sentinel, known := sentinels[name]
		if !known {
			t.Fatalf("no sentinel for %s", name)
		}
		var reply struct {
			Message string `json:"message"`
			Code    string `json:"code"`
		}
		if err := json.Unmarshal(wireFixture(t, "replies/"+name), &reply); err != nil {
			t.Fatal(err)
		}
		socket.reply("replies/" + name)
		err := client.Heartbeat(context.Background(), wireRunID, wireClaim, wireLeaseMs)
		var serverErr *ServerError
		if !errors.As(err, &serverErr) || serverErr.Message != reply.Message || serverErr.Code != reply.Code {
			t.Fatalf("%s: %#v", name, err)
		}
		for _, other := range all {
			if errors.Is(err, other) != (other == sentinel) {
				t.Fatalf("%s: errors.Is(%v) = %v", name, other, errors.Is(err, other))
			}
		}
		if isStaleOwner(err) != (sentinel == ErrRunNotOwned) {
			t.Fatalf("%s: stale owner = %v", name, isStaleOwner(err))
		}
		socket.take()
	}
}
