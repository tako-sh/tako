package tako

import (
	"context"
	"errors"
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
	for range 2 {
		if err := api.Run("charge", &result, body); err != nil {
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
