package tako

import (
	"context"
	"encoding/json"
	"errors"
	"net"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// MockServer emulates the tako-server enqueue socket with an in-memory
// task store — just enough to drive worker lifecycle tests end-to-end.
type mockServer struct {
	l        net.Listener
	path     string
	mu       sync.Mutex
	tasks    map[string]*Run
	idCount  int
	received []map[string]any
}

func startMockServer(t *testing.T) *mockServer {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "tako-worker-mock-")
	if err != nil {
		t.Fatalf("tempdir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	path := filepath.Join(dir, "srv.sock")
	l, err := net.Listen("unix", path)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	s := &mockServer{l: l, path: path, tasks: map[string]*Run{}}
	go s.accept()
	t.Cleanup(func() { _ = l.Close() })
	return s
}

func (s *mockServer) accept() {
	for {
		conn, err := s.l.Accept()
		if err != nil {
			return
		}
		go s.handleConn(conn)
	}
}

func (s *mockServer) handleConn(c net.Conn) {
	defer c.Close()
	buf := make([]byte, 0, 1024)
	tmp := make([]byte, 1024)
	for {
		n, err := c.Read(tmp)
		if n > 0 {
			buf = append(buf, tmp[:n]...)
			for {
				i := indexOfByte(buf, '\n')
				if i == -1 {
					break
				}
				line := buf[:i]
				buf = buf[i+1:]
				var cmd map[string]any
				if err := json.Unmarshal(line, &cmd); err != nil {
					c.Write([]byte(`{"status":"error","message":"bad json"}` + "\n"))
					continue
				}
				s.mu.Lock()
				s.received = append(s.received, cmd)
				s.mu.Unlock()
				resp := s.dispatch(cmd)
				out, _ := json.Marshal(resp)
				c.Write(append(out, '\n'))
			}
		}
		if err != nil {
			return
		}
	}
}

func indexOfByte(buf []byte, b byte) int {
	for i, v := range buf {
		if v == b {
			return i
		}
	}
	return -1
}

func (s *mockServer) seed(name string, maxAttempts uint32) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.idCount++
	id := "t" + itoa(s.idCount)
	s.tasks[id] = &Run{
		ID:          id,
		Name:        name,
		Payload:     json.RawMessage("{}"),
		Status:      "pending",
		Attempts:    0,
		MaxAttempts: maxAttempts,
		RunAtMs:     time.Now().UnixMilli(),
		StepState:   map[string]any{},
	}
	return id
}

func (s *mockServer) find(id string) *Run {
	s.mu.Lock()
	defer s.mu.Unlock()
	t := s.tasks[id]
	return t
}

func (s *mockServer) dispatch(cmd map[string]any) map[string]any {
	s.mu.Lock()
	defer s.mu.Unlock()

	switch cmd["command"] {
	case "claim_run":
		rawNames, _ := cmd["names"].([]any)
		names := make([]string, 0, len(rawNames))
		for _, v := range rawNames {
			if str, ok := v.(string); ok {
				names = append(names, str)
			}
		}
		for _, t := range s.tasks {
			if t.Status == "pending" && contains(names, t.Name) && t.RunAtMs <= time.Now().UnixMilli() {
				t.Status = "running"
				t.Attempts++
				return map[string]any{
					"status": "ok",
					"data": map[string]any{
						"id":           t.ID,
						"name":         t.Name,
						"payload":      json.RawMessage(t.Payload),
						"status":       t.Status,
						"attempts":     t.Attempts,
						"max_attempts": t.MaxAttempts,
						"run_at_ms":    t.RunAtMs,
						"step_state":   t.StepState,
					},
				}
			}
		}
		return map[string]any{"status": "ok", "data": nil}
	case "heartbeat_run":
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "save_step":
		id, _ := cmd["id"].(string)
		stepName, _ := cmd["step_name"].(string)
		if t := s.tasks[id]; t != nil {
			if t.StepState == nil {
				t.StepState = map[string]any{}
			}
			t.StepState[stepName] = cmd["result"]
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "complete_run":
		id, _ := cmd["id"].(string)
		if t := s.tasks[id]; t != nil {
			t.Status = "succeeded"
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "cancel_run":
		id, _ := cmd["id"].(string)
		if t := s.tasks[id]; t != nil {
			t.Status = "cancelled"
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "defer_run":
		id, _ := cmd["id"].(string)
		if t := s.tasks[id]; t != nil {
			t.Status = "pending"
			if w, ok := cmd["wake_at_ms"].(float64); ok {
				t.RunAtMs = int64(w)
			}
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "wait_for_event":
		id, _ := cmd["id"].(string)
		if t := s.tasks[id]; t != nil {
			t.Status = "pending"
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "fail_run":
		id, _ := cmd["id"].(string)
		finalize, _ := cmd["finalize"].(bool)
		if t := s.tasks[id]; t != nil {
			if finalize {
				t.Status = "dead"
			} else {
				t.Status = "pending"
				if next, ok := cmd["next_run_at_ms"].(float64); ok {
					t.RunAtMs = int64(next)
				}
			}
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "register_schedules":
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "enqueue_run":
		s.idCount++
		id := "e" + itoa(s.idCount)
		return map[string]any{
			"status": "ok",
			"data":   map[string]any{"id": id, "deduplicated": false},
		}
	default:
		return map[string]any{"status": "error", "message": "unknown"}
	}
}

func contains(xs []string, s string) bool {
	for _, v := range xs {
		if v == s {
			return true
		}
	}
	return false
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	buf := [20]byte{}
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	return string(buf[i:])
}

// Tests

func resetRegistry() {
	registryMu.Lock()
	registry = map[string]workflowRegistration{}
	registryStarted = false
	registryMu.Unlock()
}

func TestEnqueueSendsEnqueueTaskCommand(t *testing.T) {
	s := startMockServer(t)
	t.Setenv("TAKO_INTERNAL_SOCKET", s.path)
	t.Setenv("TAKO_APP_NAME", "test-app")
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	s.seed("send-email", 3)
	r, err := Enqueue(ctx, "send-email", map[string]any{"to": "a@b.c"}, EnqueueOpts{})
	if err != nil {
		t.Fatalf("enqueue: %v", err)
	}
	if r.ID == "" {
		t.Fatalf("empty id")
	}
}

func TestEnqueueSerializesOpts(t *testing.T) {
	s := startMockServer(t)
	t.Setenv("TAKO_INTERNAL_SOCKET", s.path)
	t.Setenv("TAKO_APP_NAME", "test-app")
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	when := time.UnixMilli(1_700_000_000_000)
	max := uint32(5)
	key := "cron:5m:0"
	_, _ = Enqueue(ctx, "w", nil, EnqueueOpts{RunAt: &when, MaxAttempts: &max, UniqueKey: &key})

	s.mu.Lock()
	defer s.mu.Unlock()
	opts, _ := s.received[0]["opts"].(map[string]any)
	if opts["run_at_ms"].(float64) != float64(when.UnixMilli()) {
		t.Fatalf("run_at_ms: %v", opts)
	}
	if opts["max_attempts"].(float64) != 5 {
		t.Fatalf("max_attempts: %v", opts)
	}
	if opts["unique_key"].(string) != "cron:5m:0" {
		t.Fatalf("unique_key: %v", opts)
	}
}

func TestWorkerRunsHandlerAndCompletes(t *testing.T) {
	resetRegistry()
	s := startMockServer(t)
	t.Setenv("TAKO_INTERNAL_SOCKET", s.path)
	t.Setenv("TAKO_APP_NAME", "test-app")

	called := atomic.Bool{}
	RegisterWorkflow("echo", func(ctx *WorkflowContext, p json.RawMessage) error {
		called.Store(true)
		return nil
	})

	id := s.seed("echo", 3)

	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	done := make(chan struct{})
	go func() {
		_ = RunWorker(ctx)
		close(done)
	}()

	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if called.Load() && s.find(id).Status == "succeeded" {
			cancel()
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	<-done

	if !called.Load() {
		t.Fatalf("handler never ran")
	}
	if s.find(id).Status != "succeeded" {
		t.Fatalf("task not succeeded: %v", s.find(id).Status)
	}
}

func TestStepRunMemoizesAcrossRetries(t *testing.T) {
	resetRegistry()
	s := startMockServer(t)
	t.Setenv("TAKO_INTERNAL_SOCKET", s.path)
	t.Setenv("TAKO_APP_NAME", "test-app")

	var aRuns, bRuns atomic.Uint32
	forceFail := atomic.Bool{}
	forceFail.Store(true)

	RegisterWorkflow("multi", func(ctx *WorkflowContext, p json.RawMessage) error {
		var got string
		if err := ctx.Step.Run("a", &got, func() (any, error) {
			aRuns.Add(1)
			return "user-1", nil
		}); err != nil {
			return err
		}
		return ctx.Step.Run("b", nil, func() (any, error) {
			bRuns.Add(1)
			if forceFail.Load() {
				return nil, errors.New("fail-b")
			}
			return got, nil
		})
	})

	s.seed("multi", 5)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	done := make(chan struct{})
	go func() {
		_ = RunWorker(ctx)
		close(done)
	}()

	// First attempt runs + fails on b.
	time.Sleep(600 * time.Millisecond)
	forceFail.Store(false)
	time.Sleep(2 * time.Second)
	cancel()
	<-done

	if aRuns.Load() != 1 {
		t.Fatalf("step a should have run exactly once, got %d", aRuns.Load())
	}
	if bRuns.Load() < 2 {
		t.Fatalf("step b should have run at least twice, got %d", bRuns.Load())
	}
}
