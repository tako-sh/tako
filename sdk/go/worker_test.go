package tako

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
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
	l            net.Listener
	path         string
	mu           sync.Mutex
	tasks        map[string]*Run
	idCount      int
	received     []map[string]any
	saveFailures map[string]int
	// Transient failures returned for the next calls of a command.
	failures map[string]int
	results  map[string]any
	errors   map[string]string
	// A canned reply for the next call of a command.
	replies map[string]map[string]any
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

	if command, _ := cmd["command"].(string); s.replies[command] != nil {
		reply := s.replies[command]
		delete(s.replies, command)
		return reply
	}
	if cmd["command"] != "claim_run" && cmd["command"] != "register_schedules" && cmd["command"] != "enqueue_run" && cmd["command"] != "signal" && cmd["command"] != "get_run_result" {
		id, _ := cmd["id"].(string)
		task := s.tasks[id]
		if task == nil || task.Status != "running" || task.WorkerID != cmd["worker_id"] {
			return map[string]any{
				"status":  "error",
				"message": "stale worker: run is no longer owned by this worker",
				"code":    "run_not_owned",
			}
		}
	}
	if command, _ := cmd["command"].(string); s.failures[command] > 0 {
		s.failures[command]--
		return map[string]any{"status": "error", "message": "storage timeout"}
	}
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
				t.WorkerID = fmt.Sprintf("%s:%s:%d", cmd["worker_id"], t.ID, t.Attempts)
				return map[string]any{
					"status": "ok",
					"data": map[string]any{
						"id":           t.ID,
						"worker_id":    t.WorkerID,
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
		if s.saveFailures[stepName] > 0 {
			s.saveFailures[stepName]--
			return map[string]any{"status": "error", "message": "checkpoint unavailable"}
		}
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
			if s.results == nil {
				s.results = map[string]any{}
			}
			s.results[id] = cmd["result"]
		}
		return map[string]any{"status": "ok", "data": map[string]any{}}
	case "get_run_result":
		id, _ := cmd["id"].(string)
		t := s.tasks[id]
		if t == nil || t.Name != cmd["name"] {
			return map[string]any{"status": "ok", "data": nil}
		}
		var runErr any
		if msg, ok := s.errors[id]; ok {
			runErr = msg
		}
		return map[string]any{
			"status": "ok",
			"data":   map[string]any{"status": t.Status, "result": s.results[id], "error": runErr},
		}
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
			if s.errors == nil {
				s.errors = map[string]string{}
			}
			s.errors[id], _ = cmd["error"].(string)
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
	_, _ = Enqueue(ctx, "w", nil, EnqueueOpts{RunAt: &when, MaxAttempts: &max, ID: &key})

	s.mu.Lock()
	defer s.mu.Unlock()
	opts, _ := s.received[0]["opts"].(map[string]any)
	if opts["run_at_ms"].(float64) != float64(when.UnixMilli()) {
		t.Fatalf("run_at_ms: %v", opts)
	}
	if opts["max_attempts"].(float64) != 5 {
		t.Fatalf("max_attempts: %v", opts)
	}
	if opts["id"] != "cron:5m:0" {
		t.Fatalf("id: %v", opts)
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

// operatorCancel does what `tako workflows cancel` does to a running run:
// it ends the run and fences the claim owner.
func (s *mockServer) operatorCancel(id string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	t := s.tasks[id]
	t.Status = "cancelled"
	t.WorkerID = ""
}

func (s *mockServer) commandsAfterClaim() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []string
	claimed := false
	for _, cmd := range s.received {
		name, _ := cmd["command"].(string)
		if claimed {
			out = append(out, name)
		}
		if name == "claim_run" && !claimed {
			claimed = true
		}
	}
	return out
}

func TestOperatorCancelStopsRunWithoutRetryingOrFailing(t *testing.T) {
	s := startMockServer(t)
	client := &Client{socketPath: s.path, app: "test-app"}
	id := s.seed("job", 3)
	var steps []string
	var mu sync.Mutex
	record := func(name string) {
		mu.Lock()
		steps = append(steps, name)
		mu.Unlock()
	}
	handlers := map[string]workflowRegistration{"job": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		opts := StepRunOpts{Retries: 3}
		opts.Backoff.Base = time.Millisecond
		err := ctx.Step.Run("charge", nil, func() (any, error) {
			record("charge")
			s.operatorCancel(ctx.RunID)
			return "receipt", nil
		}, opts)
		// Handler code that ignores the error still cannot continue.
		_ = err
		return ctx.Step.Run("ship", nil, func() (any, error) {
			record("ship")
			return nil, nil
		})
	}}}
	w := newWorker(context.Background(), client, handlers)
	w.heartbeatMs = 60_000
	task, err := client.Claim(context.Background(), w.workerID, []string{"job"}, w.leaseMs)
	if err != nil || task == nil {
		t.Fatalf("claim: %v %v", task, err)
	}
	w.execute(task)

	if fmt.Sprint(steps) != "[charge]" {
		t.Fatalf("steps after cancel: %v", steps)
	}
	if got := s.find(id).Status; got != "cancelled" {
		t.Fatalf("status: %s", got)
	}
	if got := fmt.Sprint(s.commandsAfterClaim()); got != "[save_step]" {
		t.Fatalf("worker kept writing after losing the run: %s", got)
	}
}

func TestLostLeaseStopsHandlerAtNextCheckpoint(t *testing.T) {
	s := startMockServer(t)
	client := &Client{socketPath: s.path, app: "test-app"}
	id := s.seed("job", 3)
	var second atomic.Bool
	var cancelled atomic.Bool
	handlers := map[string]workflowRegistration{"job": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		s.operatorCancel(ctx.RunID)
		select {
		case <-ctx.Done():
			cancelled.Store(true)
		case <-time.After(2 * time.Second):
		}
		return ctx.Step.Run("second", nil, func() (any, error) {
			second.Store(true)
			return nil, nil
		})
	}}}
	w := newWorker(context.Background(), client, handlers)
	w.heartbeatMs = 5
	task, err := client.Claim(context.Background(), w.workerID, []string{"job"}, w.leaseMs)
	if err != nil || task == nil {
		t.Fatalf("claim: %v %v", task, err)
	}
	w.execute(task)

	if !cancelled.Load() {
		t.Fatalf("context was not cancelled after the heartbeat lost ownership")
	}
	if second.Load() {
		t.Fatalf("a step ran after ownership was lost")
	}
	if got := s.find(id).Status; got != "cancelled" {
		t.Fatalf("status: %s", got)
	}
	for _, cmd := range s.commandsAfterClaim() {
		if cmd != "heartbeat_run" {
			t.Fatalf("unexpected write after losing the run: %s", cmd)
		}
	}
}

// A transient failure of the final write must not leave the run to lease
// reclaim, which would end a last attempt as dead.
func TestTransientFinalWriteFailuresAreRetriedWhileTheHeartbeatRuns(t *testing.T) {
	s := startMockServer(t)
	s.failures = map[string]int{"complete_run": 2, "fail_run": 2}
	client := &Client{socketPath: s.path, app: "test-app"}
	handlers := map[string]workflowRegistration{
		"ok":   {handler: func(*WorkflowContext, json.RawMessage) error { return nil }},
		"boom": {handler: func(*WorkflowContext, json.RawMessage) error { return errors.New("boom") }},
	}
	w := newWorker(context.Background(), client, handlers)
	w.heartbeatMs = 20
	for _, run := range [][2]string{{"ok", "succeeded"}, {"boom", "dead"}} {
		name, want := run[0], run[1]
		id := s.seed(name, 1)
		task, err := client.Claim(context.Background(), w.workerID, []string{name}, w.leaseMs)
		if err != nil || task == nil {
			t.Fatalf("claim: %v %v", task, err)
		}
		w.execute(task)
		if got := s.find(id).Status; got != want {
			t.Fatalf("%s: status %s, want %s", name, got, want)
		}
	}
	if s.errors["t2"] != "boom" {
		t.Fatalf("stored error: %v", s.errors)
	}
	var writes, heartbeats int
	for _, cmd := range s.commandsAfterClaim() {
		switch cmd {
		case "complete_run":
			writes++
		case "heartbeat_run":
			if writes > 0 && writes < 3 {
				heartbeats++
			}
		}
	}
	if writes != 3 || heartbeats == 0 {
		t.Fatalf("complete_run calls %d, heartbeats while retrying %d", writes, heartbeats)
	}
}

func TestScheduleRegistrationCarriesTheRetryBudget(t *testing.T) {
	resetRegistry()
	s := startMockServer(t)
	t.Setenv("TAKO_INTERNAL_SOCKET", s.path)
	t.Setenv("TAKO_APP_NAME", "test-app")
	noop := func(*WorkflowContext, json.RawMessage) error { return nil }
	RegisterWorkflow("nightly", noop, WithSchedule("0 0 * * *"), WithMaxAttempts(5))
	RegisterWorkflow("hourly", noop, WithSchedule("0 * * * *"))

	// Schedules are registered before the claim loop starts.
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	if err := RunWorker(ctx); err != nil {
		t.Fatalf("run worker: %v", err)
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if len(s.received) == 0 || s.received[0]["command"] != "register_schedules" {
		t.Fatalf("commands: %v", s.received)
	}
	got := map[string]any{}
	for _, raw := range s.received[0]["schedules"].([]any) {
		schedule := raw.(map[string]any)
		got[schedule["name"].(string)] = schedule
	}
	want := map[string]any{
		"nightly": map[string]any{"name": "nightly", "cron": "0 0 * * *", "max_attempts": float64(5)},
		// Without WithMaxAttempts the server default budget applies.
		"hourly": map[string]any{"name": "hourly", "cron": "0 * * * *"},
	}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("schedules: %v", got)
	}
}

func (s *mockServer) reply(command string, reply map[string]any) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.replies == nil {
		s.replies = map[string]map[string]any{}
	}
	s.replies[command] = reply
}

// The server's id-conflict message echoes the caller's run ID, so its text
// can contain anything. Only the run_not_owned code on a worker write means
// ownership was lost.
func TestUnrelatedErrorMentioningStaleWorkerDoesNotAbandonTheRun(t *testing.T) {
	s := startMockServer(t)
	client := &Client{socketPath: s.path, app: "test-app"}
	id := s.seed("parent", 1)
	s.reply("enqueue_run", map[string]any{
		"status":  "error",
		"message": "workflow run ID 'stale worker' already belongs to a different workflow or payload",
		"code":    "run_id_conflict",
	})
	childID := "stale worker"
	var enqueueErr error
	handlers := map[string]workflowRegistration{"parent": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		_, enqueueErr = client.Enqueue(ctx.ctx, "child", nil, EnqueueOpts{ID: &childID})
		return enqueueErr
	}}}
	w := newWorker(context.Background(), client, handlers)
	w.heartbeatMs = 60_000
	task, err := client.Claim(context.Background(), w.workerID, []string{"parent"}, w.leaseMs)
	if err != nil || task == nil {
		t.Fatalf("claim: %v %v", task, err)
	}
	w.execute(task)

	if !errors.Is(enqueueErr, ErrRunIDConflict) || errors.Is(enqueueErr, ErrRunNotOwned) {
		t.Fatalf("enqueue error: %v", enqueueErr)
	}
	if got := s.find(id).Status; got != "dead" {
		t.Fatalf("the run was abandoned instead of failed: %s", got)
	}
	if !contains(s.commandsAfterClaim(), "fail_run") {
		t.Fatalf("no fail_run after the handler error: %v", s.commandsAfterClaim())
	}
}

func TestOnlyACodedWorkerWriteRejectionIsLostOwnership(t *testing.T) {
	s := startMockServer(t)
	client := &Client{socketPath: s.path, app: "test-app"}
	ctx := context.Background()

	s.reply("heartbeat_run", map[string]any{"status": "error", "message": "stale worker"})
	textOnly := client.Heartbeat(ctx, "r1", "w1", 1000)
	var serverErr *ServerError
	if textOnly == nil || isStaleOwner(textOnly) || errors.Is(textOnly, ErrRunNotOwned) ||
		!errors.As(textOnly, &serverErr) || serverErr.Code != "" {
		t.Fatalf("an uncoded rejection read as lost ownership: %#v", textOnly)
	}

	// The code counts only on worker writes, not on any reply.
	s.reply("enqueue_run", map[string]any{"status": "error", "message": "x", "code": "run_not_owned"})
	_, enqueueErr := client.Enqueue(ctx, "job", nil, EnqueueOpts{})
	if !errors.Is(enqueueErr, ErrRunNotOwned) || isStaleOwner(enqueueErr) {
		t.Fatalf("enqueue error: %#v", enqueueErr)
	}

	stale := client.Heartbeat(ctx, "r1", "w1", 1000)
	if !isStaleOwner(stale) || !errors.Is(stale, ErrRunNotOwned) {
		t.Fatalf("a run_not_owned worker write was not lost ownership: %#v", stale)
	}
}

func TestServerErrorsMatchTheirSentinel(t *testing.T) {
	for code, sentinel := range errorCodes {
		err := fmt.Errorf("wrapped: %w", &ServerError{Code: code, Message: "m"})
		if !errors.Is(err, sentinel) {
			t.Fatalf("%s does not match its sentinel", code)
		}
		for other, otherSentinel := range errorCodes {
			if other != code && errors.Is(err, otherSentinel) {
				t.Fatalf("%s matches %s", code, other)
			}
		}
	}
	for _, code := range []string{"", "from_a_newer_server"} {
		err := &ServerError{Code: code, Message: "m"}
		for _, sentinel := range errorCodes {
			if errors.Is(err, sentinel) {
				t.Fatalf("code %q matched a sentinel", code)
			}
		}
	}
	if len(errorCodes) != 7 {
		t.Fatalf("expected the 7 server codes, got %d", len(errorCodes))
	}
}
