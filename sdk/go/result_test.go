package tako

import (
	"context"
	"encoding/json"
	"errors"
	"reflect"
	"testing"
	"time"
)

func workerEnv(t *testing.T) *mockServer {
	t.Helper()
	resetRegistry()
	s := startMockServer(t)
	t.Setenv("TAKO_INTERNAL_SOCKET", s.path)
	t.Setenv("TAKO_APP_NAME", "test-app")
	return s
}

func (s *mockServer) status(id string) string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.tasks[id].Status
}

// runUntil runs the worker until done reports true, then drains it.
func runUntil(t *testing.T, done func() bool) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	finished := make(chan struct{})
	go func() {
		_ = RunWorker(ctx)
		close(finished)
	}()
	for ctx.Err() == nil && !done() {
		time.Sleep(10 * time.Millisecond)
	}
	cancel()
	<-finished
	if !done() {
		t.Fatal("worker did not reach the expected state")
	}
}

func TestWorkerCompletesWithHandlerResult(t *testing.T) {
	s := workerEnv(t)
	RegisterWorkflow("report", func(ctx *WorkflowContext, _ json.RawMessage) error {
		if err := ctx.SetResult("draft"); err != nil {
			return err
		}
		// The last call wins.
		return ctx.SetResult(map[string]any{"url": "report.pdf", "count": 42})
	})
	id := s.seed("report", 3)
	runUntil(t, func() bool { return s.status(id) == "succeeded" })

	s.mu.Lock()
	sent := s.results[id]
	s.mu.Unlock()
	want := map[string]any{"url": "report.pdf", "count": float64(42)}
	if !reflect.DeepEqual(sent, want) {
		t.Fatalf("complete_run result = %#v, want %#v", sent, want)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	state, err := Result(ctx, "report", id)
	if err != nil {
		t.Fatal(err)
	}
	if state == nil || state.Status != "succeeded" || state.Error != "" {
		t.Fatalf("state = %+v", state)
	}
	var got struct {
		URL   string `json:"url"`
		Count int    `json:"count"`
	}
	if err := json.Unmarshal(state.Result, &got); err != nil {
		t.Fatal(err)
	}
	if got.URL != "report.pdf" || got.Count != 42 {
		t.Fatalf("result = %+v", got)
	}
}

func TestWorkerCompletesWithNullWhenNoResultIsSet(t *testing.T) {
	s := workerEnv(t)
	RegisterWorkflow("quiet", func(*WorkflowContext, json.RawMessage) error { return nil })
	id := s.seed("quiet", 3)
	runUntil(t, func() bool { return s.status(id) == "succeeded" })

	s.mu.Lock()
	defer s.mu.Unlock()
	if sent, ok := s.results[id]; !ok || sent != nil {
		t.Fatalf("complete_run result = %#v, want null", sent)
	}
}

func TestSetResultRejectsValuesThatDoNotEncode(t *testing.T) {
	ctx := &WorkflowContext{}
	if err := ctx.SetResult(map[string]int{"count": 1}); err != nil {
		t.Fatal(err)
	}
	if err := ctx.SetResult(make(chan int)); err == nil {
		t.Fatal("expected an encoding error")
	}
	// A rejected value leaves the previous result in place.
	if string(ctx.result) != `{"count":1}` {
		t.Fatalf("result = %s", ctx.result)
	}
}

func TestResultReportsMissingAndFailedRuns(t *testing.T) {
	s := workerEnv(t)
	RegisterWorkflow("broken", func(ctx *WorkflowContext, _ json.RawMessage) error {
		return ctx.Fail(errors.New("card declined"))
	})
	id := s.seed("broken", 3)
	runUntil(t, func() bool { return s.status(id) == "dead" })

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	state, err := Result(ctx, "broken", id)
	if err != nil {
		t.Fatal(err)
	}
	if state == nil || state.Status != "dead" || state.Error != "card declined" {
		t.Fatalf("state = %+v", state)
	}
	if string(state.Result) != "null" {
		t.Fatalf("result = %s", state.Result)
	}
	// Results are scoped to the workflow that owns the run.
	for _, lookup := range [][2]string{{"other", id}, {"broken", "missing"}} {
		state, err := Result(ctx, lookup[0], lookup[1])
		if err != nil || state != nil {
			t.Fatalf("Result(%q, %q) = %+v, %v", lookup[0], lookup[1], state, err)
		}
	}
}

// The stored budget decides when a run dies, so a per-run override and the
// server's accounting cannot disagree with the worker's registration.
func TestWorkerUsesTheRunsStoredRetryBudget(t *testing.T) {
	for _, tc := range []struct {
		name       string
		registered uint32
		stored     uint32
		want       string
	}{
		{"stored budget remains", 1, 3, "pending"},
		{"stored budget exhausted", 5, 1, "dead"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			s := workerEnv(t)
			RegisterWorkflow("flaky", func(*WorkflowContext, json.RawMessage) error {
				return errors.New("boom")
			}, WithMaxAttempts(tc.registered), WithBackoff(time.Hour, time.Hour))
			id := s.seed("flaky", tc.stored)
			runUntil(t, func() bool {
				s.mu.Lock()
				defer s.mu.Unlock()
				_, failed := s.errors[id]
				return failed
			})
			if got := s.status(id); got != tc.want {
				t.Fatalf("status = %s, want %s", got, tc.want)
			}
		})
	}
}

func TestEnqueueStampsTheRegisteredAttemptBudget(t *testing.T) {
	s := workerEnv(t)
	RegisterWorkflow("budgeted", func(*WorkflowContext, json.RawMessage) error { return nil },
		WithMaxAttempts(5))
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	override := uint32(2)
	for _, call := range []struct {
		name string
		opts EnqueueOpts
	}{
		{"budgeted", EnqueueOpts{}},
		{"budgeted", EnqueueOpts{MaxAttempts: &override}},
		{"unregistered", EnqueueOpts{}},
	} {
		if _, err := Enqueue(ctx, call.name, nil, call.opts); err != nil {
			t.Fatal(err)
		}
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	var sent []any
	for _, cmd := range s.received {
		opts, _ := cmd["opts"].(map[string]any)
		sent = append(sent, opts["max_attempts"])
	}
	if want := []any{float64(5), float64(2), nil}; !reflect.DeepEqual(sent, want) {
		t.Fatalf("max_attempts = %#v, want %#v", sent, want)
	}
}
