// Package tako provides the runtime SDK for Tako-deployed Go applications.
//
// All durable-run state is owned by tako-server. The SDK is a thin RPC
// client over a shared unix socket (path in TAKO_INTERNAL_SOCKET); every
// command carries the app name (TAKO_APP_NAME) so one socket handles
// every deployed app. The SDK has no SQLite dependency.
package tako

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"time"
)

const (
	// WorkflowSocketEnv is the environment variable containing the shared internal RPC socket path.
	WorkflowSocketEnv = "TAKO_INTERNAL_SOCKET"
	// AppNameEnv is the environment variable containing the current Tako app name.
	AppNameEnv = "TAKO_APP_NAME"
)

// EnqueueOpts controls per-enqueue behavior. Nil fields fall back to
// server-side defaults (runAt = now, maxAttempts = 3, no dedup).
type EnqueueOpts struct {
	// RunAt schedules the run for a future time. Nil means now.
	RunAt *time.Time
	// MaxAttempts is the total run-level attempt budget. Nil means the server default.
	MaxAttempts *uint32
	// UniqueKey deduplicates against an existing non-terminal run with the same key.
	UniqueKey *string
}

// EnqueueResult is the server's response.
type EnqueueResult struct {
	// ID is the workflow run id.
	ID string
	// Deduplicated is true when an existing run was reused for the enqueue request.
	Deduplicated bool
}

// Enqueue dispatches a run of the named workflow.
func Enqueue(ctx context.Context, name string, payload any, opts EnqueueOpts) (*EnqueueResult, error) {
	client, err := ClientFromEnv()
	if err != nil {
		return nil, err
	}
	return client.Enqueue(ctx, name, payload, opts)
}

// Client is a thin RPC wrapper over the shared workflow socket. All
// outbound commands include the app name so one socket can route for many
// apps.
type Client struct {
	socketPath string
	app        string
}

// NewClient constructs a client rooted at the given socket path and app.
func NewClient(socketPath, app string) *Client {
	return &Client{socketPath: socketPath, app: app}
}

// ClientFromEnv reads TAKO_INTERNAL_SOCKET and TAKO_APP_NAME.
func ClientFromEnv() (*Client, error) {
	sock := os.Getenv(WorkflowSocketEnv)
	if sock == "" {
		return nil, errors.New("tako: " + WorkflowSocketEnv + " is not set")
	}
	app := os.Getenv(AppNameEnv)
	if app == "" {
		return nil, errors.New("tako: " + AppNameEnv + " is not set")
	}
	return NewClient(sock, app), nil
}

// Enqueue dispatches a run of the named workflow through this client.
func (c *Client) Enqueue(ctx context.Context, name string, payload any, opts EnqueueOpts) (*EnqueueResult, error) {
	if payload == nil {
		payload = struct{}{}
	}
	cmd := map[string]any{
		"command": "enqueue_run",
		"app":     c.app,
		"name":    name,
		"payload": payload,
		"opts":    optsToWire(opts),
	}
	data, err := c.call(ctx, cmd)
	if err != nil {
		return nil, err
	}
	var ok struct {
		ID           string `json:"id"`
		Deduplicated bool   `json:"deduplicated"`
	}
	if err := json.Unmarshal(data, &ok); err != nil {
		return nil, fmt.Errorf("tako: parse enqueue response: %w", err)
	}
	return &EnqueueResult{ID: ok.ID, Deduplicated: ok.Deduplicated}, nil
}

// RegisterSchedules sends the list of cron schedules to the server.
func (c *Client) RegisterSchedules(ctx context.Context, schedules []ScheduleSpec) error {
	_, err := c.call(ctx, map[string]any{
		"command":   "register_schedules",
		"app":       c.app,
		"schedules": schedules,
	})
	return err
}

// ScheduleSpec is one workflow+cron pair sent on worker startup.
type ScheduleSpec struct {
	// Name is the workflow name.
	Name string `json:"name"`
	// Cron is a 5-field cron expression.
	Cron string `json:"cron"`
}

// Run is the server-returned run payload from ClaimRun.
type Run struct {
	// ID is the workflow run id.
	ID string `json:"id"`
	// Name is the workflow name.
	Name string `json:"name"`
	// Payload is the raw JSON payload passed to the handler.
	Payload json.RawMessage `json:"payload"`
	// Status is the current run status.
	Status string `json:"status"`
	// Attempts is the number of attempts already made.
	Attempts uint32 `json:"attempts"`
	// MaxAttempts is the total run-level attempt budget.
	MaxAttempts uint32 `json:"max_attempts"`
	// RunAtMs is the scheduled run time in Unix milliseconds.
	RunAtMs int64 `json:"run_at_ms"`
	// StepState contains persisted step results for this run.
	StepState map[string]any `json:"step_state"`
}

// Claim atomically claims the oldest eligible run and bumps attempts.
// Returns nil when nothing is due.
func (c *Client) Claim(ctx context.Context, workerID string, names []string, leaseMs uint64) (*Run, error) {
	data, err := c.call(ctx, map[string]any{
		"command":   "claim_run",
		"app":       c.app,
		"worker_id": workerID,
		"names":     names,
		"lease_ms":  leaseMs,
	})
	if err != nil {
		return nil, err
	}
	if len(data) == 0 || string(data) == "null" {
		return nil, nil
	}
	var t Run
	if err := json.Unmarshal(data, &t); err != nil {
		return nil, fmt.Errorf("tako: parse run: %w", err)
	}
	if t.StepState == nil {
		t.StepState = map[string]any{}
	}
	return &t, nil
}

// Heartbeat extends the lease on a running run. `workerId` must match the
// `worker_id` that claimed the run; if the lease was reclaimed by another
// worker, the call returns an error.
func (c *Client) Heartbeat(ctx context.Context, id, workerID string, leaseMs uint64) error {
	_, err := c.call(ctx, map[string]any{
		"command":   "heartbeat_run",
		"app":       c.app,
		"id":        id,
		"worker_id": workerID,
		"lease_ms":  leaseMs,
	})
	return err
}

// SaveStep persists a single completed step result. Guarded by `workerId`
// so a stale worker (past its lease) can't scribble into a different
// worker's run. First-write-wins on (run_id, step_name).
func (c *Client) SaveStep(ctx context.Context, id, workerID, stepName string, result any) error {
	_, err := c.call(ctx, map[string]any{
		"command":   "save_step",
		"app":       c.app,
		"id":        id,
		"worker_id": workerID,
		"step_name": stepName,
		"result":    result,
	})
	return err
}

// Complete marks the run succeeded. Guarded by `workerId`.
func (c *Client) Complete(ctx context.Context, id, workerID string) error {
	_, err := c.call(ctx, map[string]any{
		"command":   "complete_run",
		"app":       c.app,
		"id":        id,
		"worker_id": workerID,
	})
	return err
}

// Cancel ends the run cleanly as `cancelled` (no retries). Guarded.
func (c *Client) Cancel(ctx context.Context, id, workerID string, reason *string) error {
	body := map[string]any{
		"command":   "cancel_run",
		"app":       c.app,
		"id":        id,
		"worker_id": workerID,
		"reason":    nil,
	}
	if reason != nil {
		body["reason"] = *reason
	}
	_, err := c.call(ctx, body)
	return err
}

// Defer parks the run for later. nil wakeAt = parked indefinitely.
// Does not consume retry budget. Guarded by `workerId`.
func (c *Client) Defer(ctx context.Context, id, workerID string, wakeAt *time.Time) error {
	body := map[string]any{
		"command":    "defer_run",
		"app":        c.app,
		"id":         id,
		"worker_id":  workerID,
		"wake_at_ms": nil,
	}
	if wakeAt != nil {
		body["wake_at_ms"] = wakeAt.UnixMilli()
	}
	_, err := c.call(ctx, body)
	return err
}

// WaitForEvent parks the run on a named event. Resumes when a matching
// Signal arrives or timeoutAt elapses. Guarded by `workerId`.
func (c *Client) WaitForEvent(
	ctx context.Context, id, workerID, stepName, eventName string, timeoutAt *time.Time,
) error {
	body := map[string]any{
		"command":       "wait_for_event",
		"app":           c.app,
		"id":            id,
		"worker_id":     workerID,
		"step_name":     stepName,
		"event_name":    eventName,
		"timeout_at_ms": nil,
	}
	if timeoutAt != nil {
		body["timeout_at_ms"] = timeoutAt.UnixMilli()
	}
	_, err := c.call(ctx, body)
	return err
}

// Signal delivers an event payload, waking every parked WaitForEvent with
// matching name. Returns the number of runs woken.
func (c *Client) Signal(ctx context.Context, eventName string, payload any) (uint64, error) {
	data, err := c.call(ctx, map[string]any{
		"command":    "signal",
		"app":        c.app,
		"event_name": eventName,
		"payload":    payload,
	})
	if err != nil {
		return 0, err
	}
	var resp struct {
		Woken uint64 `json:"woken"`
	}
	if err := json.Unmarshal(data, &resp); err != nil {
		return 0, fmt.Errorf("tako: parse signal response: %w", err)
	}
	return resp.Woken, nil
}

// Signal is a top-level convenience that uses the client from env.
func Signal(ctx context.Context, eventName string, payload any) (uint64, error) {
	c, err := ClientFromEnv()
	if err != nil {
		return 0, err
	}
	return c.Signal(ctx, eventName, payload)
}

// Fail records a failure. When finalize is true the run becomes dead;
// otherwise it goes back to pending with nextRunAt as its new run_at.
// Guarded by `workerId`.
func (c *Client) Fail(
	ctx context.Context, id, workerID, errMsg string, nextRunAt *time.Time, finalize bool,
) error {
	body := map[string]any{
		"command":   "fail_run",
		"app":       c.app,
		"id":        id,
		"worker_id": workerID,
		"error":     errMsg,
		"finalize":  finalize,
	}
	if nextRunAt != nil {
		body["next_run_at_ms"] = nextRunAt.UnixMilli()
	} else {
		body["next_run_at_ms"] = nil
	}
	_, err := c.call(ctx, body)
	return err
}

func optsToWire(o EnqueueOpts) map[string]any {
	w := map[string]any{}
	if o.RunAt != nil {
		w["run_at_ms"] = o.RunAt.UnixMilli()
	}
	if o.MaxAttempts != nil {
		w["max_attempts"] = *o.MaxAttempts
	}
	if o.UniqueKey != nil {
		w["unique_key"] = *o.UniqueKey
	}
	return w
}

type wireResponse struct {
	Status  string          `json:"status"`
	Data    json.RawMessage `json:"data,omitempty"`
	Message string          `json:"message,omitempty"`
}

func (c *Client) call(ctx context.Context, cmd map[string]any) (json.RawMessage, error) {
	d := net.Dialer{Timeout: 5 * time.Second}
	conn, err := d.DialContext(ctx, "unix", c.socketPath)
	if err != nil {
		return nil, fmt.Errorf("tako: dial workflow socket: %w", err)
	}
	defer conn.Close()
	if deadline, ok := ctx.Deadline(); ok {
		_ = conn.SetDeadline(deadline)
	} else {
		_ = conn.SetDeadline(time.Now().Add(30 * time.Second))
	}

	body, err := json.Marshal(cmd)
	if err != nil {
		return nil, fmt.Errorf("tako: marshal: %w", err)
	}
	body = append(body, '\n')
	if _, err := conn.Write(body); err != nil {
		return nil, fmt.Errorf("tako: write: %w", err)
	}

	r := bufio.NewReader(conn)
	line, err := r.ReadBytes('\n')
	if err != nil {
		return nil, fmt.Errorf("tako: read: %w", err)
	}
	var resp wireResponse
	if err := json.Unmarshal(line, &resp); err != nil {
		return nil, fmt.Errorf("tako: parse response: %w", err)
	}
	if resp.Status != "ok" {
		msg := resp.Message
		if msg == "" {
			msg = "rpc failed"
		}
		return nil, errors.New("tako: " + msg)
	}
	return resp.Data, nil
}
