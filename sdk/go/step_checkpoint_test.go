package tako

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"
)

func checkpointAPI(t *testing.T, state map[string]any, failedStep string) (*StepAPI, *mockServer) {
	t.Helper()
	server := startMockServer(t)
	server.seed("checkpoint", 3)
	server.mu.Lock()
	server.saveFailures = map[string]int{failedStep: 1}
	server.mu.Unlock()
	client := NewClient(server.path, "test-app")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	t.Cleanup(cancel)
	run, err := client.Claim(ctx, "worker", []string{"checkpoint"}, 30_000)
	if err != nil || run == nil {
		t.Fatalf("claim: %v, %v", run, err)
	}
	return &StepAPI{client: client, ctx: ctx, runID: run.ID, workerID: run.WorkerID, state: state}, server
}

// nextExecution models the handler running again over the same saved steps.
func nextExecution(api *StepAPI) *StepAPI {
	return &StepAPI{client: api.client, ctx: api.ctx, runID: api.runID, workerID: api.workerID, state: api.state}
}

func assertReuseError(t *testing.T, err error, name string) {
	t.Helper()
	var failed *failSignal
	if !errors.As(err, &failed) {
		t.Fatalf("expected a permanent failure for reused step %q, got %v", name, err)
	}
	message := failed.err.Error()
	for _, want := range []string{`"` + name + `"`, "unique within a run", `"` + name + `:1"`} {
		if !strings.Contains(message, want) {
			t.Fatalf("error %q does not mention %s", message, want)
		}
	}
}

func TestStepNameReusedWithinOneExecutionFails(t *testing.T) {
	api, _ := checkpointAPI(t, map[string]any{}, "")
	var processed []string
	process := func(item string) func() (any, error) {
		return func() (any, error) { processed = append(processed, item); return item, nil }
	}
	var got string
	if err := api.Run("process", &got, process("first")); err != nil || got != "first" {
		t.Fatalf("first use: %q, %v", got, err)
	}
	got = ""
	assertReuseError(t, api.Run("process", &got, process("second")), "process")
	if got != "" || len(processed) != 1 {
		t.Fatalf("reuse returned %q after running %v", got, processed)
	}
}

func TestReplayedStepReturnsItsResultOncePerExecution(t *testing.T) {
	api, _ := checkpointAPI(t, map[string]any{"charge": "receipt-1"}, "")
	calls := 0
	body := func() (any, error) { calls++; return "receipt-2", nil }
	for _, execution := range []*StepAPI{api, nextExecution(api)} {
		var got string
		if err := execution.Run("charge", &got, body); err != nil || got != "receipt-1" {
			t.Fatalf("replay: %q, %v", got, err)
		}
		assertReuseError(t, execution.Run("charge", &got, body), "charge")
	}
	if calls != 0 {
		t.Fatalf("replayed step executed %d times", calls)
	}
}

func TestRunSleepAndWaitForShareOneStepNamespace(t *testing.T) {
	type use struct {
		kind string
		call func(*StepAPI) error
	}
	uses := []use{
		{"run", func(api *StepAPI) error {
			return api.Run("gate", nil, func() (any, error) { return "again", nil })
		}},
		{"sleep", func(api *StepAPI) error { return api.Sleep("gate", 0) }},
		{"waitFor", func(api *StepAPI) error { return api.WaitFor("gate", nil, 0) }},
	}
	for _, first := range uses {
		for _, second := range uses {
			t.Run(first.kind+" then "+second.kind, func(t *testing.T) {
				// State saved by an earlier execution lets every kind of first use return.
				state := map[string]any{"gate": float64(1)}
				if first.kind == "sleep" {
					state = map[string]any{"__sleep:gate": map[string]any{"wakeAt": float64(0)}, "gate": true}
				}
				api, server := checkpointAPI(t, state, "")
				if err := first.call(api); err != nil {
					t.Fatal(err)
				}
				assertReuseError(t, second.call(api), "gate")
				server.mu.Lock()
				defer server.mu.Unlock()
				if saved := len(server.tasks[api.runID].StepState); saved != 0 {
					t.Fatalf("reuse saved %d steps", saved)
				}
			})
		}
	}
}

func TestWaitThatParksDoesNotConsumeItsNameForTheNextExecution(t *testing.T) {
	api, _ := checkpointAPI(t, map[string]any{}, "")
	var parked *waitSignal
	if err := api.WaitFor("approval", nil, 0); !errors.As(err, &parked) {
		t.Fatalf("expected the run to park, got %v", err)
	}
	if parked.timeoutAt != nil {
		t.Fatalf("zero duration must wait indefinitely: %v", parked.timeoutAt)
	}
	api.state["approval"] = map[string]any{"approved": true}
	var decision struct {
		Approved bool `json:"approved"`
	}
	if err := nextExecution(api).WaitFor("approval", &decision, 0); err != nil || !decision.Approved {
		t.Fatalf("resume: %+v, %v", decision, err)
	}
}

func TestReusedStepNameKillsTheRunWithoutSpendingRetries(t *testing.T) {
	s := workerEnv(t)
	var processed []string
	RegisterWorkflow("batch", func(ctx *WorkflowContext, _ json.RawMessage) error {
		for _, item := range []string{"a", "b", "c"} {
			if err := ctx.Step.Run("process", nil, func() (any, error) {
				processed = append(processed, item)
				return item, nil
			}); err != nil {
				return err
			}
		}
		return nil
	})
	id := s.seed("batch", 5)
	runUntil(t, func() bool { return s.status(id) != "pending" && s.status(id) != "running" })

	s.mu.Lock()
	defer s.mu.Unlock()
	if task := s.tasks[id]; task.Status != "dead" || task.Attempts != 1 {
		t.Fatalf("status = %s after %d attempts", task.Status, task.Attempts)
	}
	if !strings.Contains(s.errors[id], `step name "process" was used more than once`) {
		t.Fatalf("stored error = %q", s.errors[id])
	}
	if len(processed) != 1 {
		t.Fatalf("processed = %v", processed)
	}
}

func TestStepCheckpointFailureDoesNotCacheSuccess(t *testing.T) {
	api, server := checkpointAPI(t, map[string]any{}, "charge")
	calls := 0
	body := func() (any, error) { calls++; return calls, nil }
	var result int
	if err := api.Run("charge", &result, body); err == nil {
		t.Fatal("expected checkpoint failure")
	}
	if _, ok := api.state["charge"]; ok {
		t.Fatal("failed checkpoint was cached")
	}
	for _, execution := range []*StepAPI{api, nextExecution(api)} {
		if err := execution.Run("charge", &result, body); err != nil {
			t.Fatal(err)
		}
		if result != 2 {
			t.Fatalf("result = %d", result)
		}
	}
	if calls != 2 {
		t.Fatalf("step executed %d times", calls)
	}
	server.mu.Lock()
	defer server.mu.Unlock()
	if server.tasks[api.runID].StepState["charge"] != float64(2) {
		t.Fatal("retry did not persist the result")
	}
}

func TestSleepCheckpointFailureDoesNotSkipPersistence(t *testing.T) {
	api, server := checkpointAPI(t, map[string]any{}, "__sleep:later")
	if err := api.Sleep("later", time.Minute); err == nil {
		t.Fatal("expected checkpoint failure")
	}
	if _, ok := api.state["__sleep:later"]; ok {
		t.Fatal("failed marker was cached")
	}
	var deferred *deferSignal
	if err := api.Sleep("later", time.Minute); !errors.As(err, &deferred) {
		t.Fatalf("expected defer, got %v", err)
	}
	server.mu.Lock()
	defer server.mu.Unlock()
	if _, ok := server.tasks[api.runID].StepState["__sleep:later"]; !ok {
		t.Fatal("retry did not persist sleep marker")
	}
}

func TestSleepCompletionCheckpointFailureIsRetried(t *testing.T) {
	for _, hydrated := range []bool{false, true} {
		t.Run(map[bool]string{false: "inline", true: "resumed"}[hydrated], func(t *testing.T) {
			state := map[string]any{}
			if hydrated {
				state["__sleep:pause"] = map[string]any{"wakeAt": float64(0)}
			}
			api, server := checkpointAPI(t, state, "pause")
			if err := api.Sleep("pause", 0); err == nil {
				t.Fatal("expected checkpoint failure")
			}
			if _, ok := state["pause"]; ok {
				t.Fatal("failed completion was cached")
			}
			if err := api.Sleep("pause", 0); err != nil {
				t.Fatal(err)
			}
			server.mu.Lock()
			defer server.mu.Unlock()
			if server.tasks[api.runID].StepState["pause"] != true {
				t.Fatal("retry did not persist completion")
			}
		})
	}
}
