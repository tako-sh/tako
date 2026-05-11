package tako

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"math/rand"
	"os"
	"sync"
	"sync/atomic"
	"time"
)

// Handler is the function signature a workflow handler must satisfy.
type Handler func(ctx *WorkflowContext, payload json.RawMessage) error

// WorkflowContext is passed to handlers during execution.
type WorkflowContext struct {
	// RunID is the unique id for the current workflow run.
	RunID string
	// WorkflowName is the registered workflow name.
	WorkflowName string
	// Attempts is the number of attempts already made for this run.
	Attempts uint32
	// Step exposes checkpointed step helpers for this run.
	Step *StepAPI
	ctx  context.Context
}

// Done returns the context's cancellation channel.
func (c *WorkflowContext) Done() <-chan struct{} { return c.ctx.Done() }

// Bail returns a sentinel error. Returning it ends the run cleanly as
// `cancelled` (no retries). To exit successfully early, just return nil
// from the handler.
func (c *WorkflowContext) Bail(reason string) error {
	return &bailSignal{reason: reason}
}

// Fail returns a sentinel error. Returning it marks the run `dead`
// immediately, skipping any remaining retry budget.
func (c *WorkflowContext) Fail(err error) error {
	if err == nil {
		err = errors.New("tako: ctx.Fail with nil error")
	}
	return &failSignal{err: err}
}

type bailSignal struct{ reason string }

func (b *bailSignal) Error() string { return "tako: bail: " + b.reason }

type failSignal struct{ err error }

func (f *failSignal) Error() string { return "tako: fail: " + f.err.Error() }
func (f *failSignal) Unwrap() error { return f.err }

type deferSignal struct{ wakeAt *time.Time }

func (d *deferSignal) Error() string { return "tako: defer signal" }

type waitSignal struct {
	stepName  string
	eventName string
	timeoutAt *time.Time
}

func (w *waitSignal) Error() string { return "tako: wait signal" }

// StepAPI is the checkpointed step runner.
type StepAPI struct {
	client   *Client
	ctx      context.Context
	runID    string
	workerID string
	state    map[string]any
	mu       sync.Mutex
}

// StepRunOpts configures a single step.Run invocation.
type StepRunOpts struct {
	// Retries is the in-step retry budget (default 0 = no internal retries).
	Retries uint32
	// Backoff configures in-step retry delays.
	Backoff struct {
		// Base is the initial retry delay. Zero means 1s.
		Base time.Duration
		// Max is the retry delay cap. Zero means 30s.
		Max time.Duration
	}
	// NoRetry fails the run immediately on any fn error, skipping in-step and run-level retries.
	NoRetry bool
}

const inlineSleepThreshold = 30 * time.Second

// Run executes fn the first time and persists its return value. On retry
// the stored value is returned without calling fn.
//
// At-least-once: fn may run more than once on crashes — make it
// idempotent (Stripe idempotency keys, upsert not insert, etc.).
func (s *StepAPI) Run(name string, out any, fn func() (any, error), opts ...StepRunOpts) error {
	s.mu.Lock()
	if cached, ok := s.state[name]; ok {
		s.mu.Unlock()
		return assignCached(cached, out)
	}
	s.mu.Unlock()

	var o StepRunOpts
	if len(opts) > 0 {
		o = opts[0]
	}
	base := o.Backoff.Base
	if base == 0 {
		base = time.Second
	}
	max := o.Backoff.Max
	if max == 0 {
		max = 30 * time.Second
	}
	attempts := o.Retries + 1

	var lastErr error
	for attempt := uint32(1); attempt <= attempts; attempt++ {
		value, err := fn()
		if err == nil {
			s.mu.Lock()
			s.state[name] = value
			s.mu.Unlock()
			if perr := s.client.SaveStep(s.ctx, s.runID, s.workerID, name, value); perr != nil {
				return fmt.Errorf("persist step state: %w", perr)
			}
			return assignCached(value, out)
		}
		// Control signals propagate immediately — never retry them.
		if isControlSignal(err) {
			return err
		}
		lastErr = err
		if o.NoRetry {
			return &failSignal{err: err}
		}
		if attempt < attempts {
			time.Sleep(expBackoff(attempt, base, max))
		}
	}
	return lastErr
}

func isControlSignal(err error) bool {
	var bs *bailSignal
	var fs *failSignal
	var ds *deferSignal
	var ws *waitSignal
	return errors.As(err, &bs) || errors.As(err, &fs) ||
		errors.As(err, &ds) || errors.As(err, &ws)
}

// Sleep durably waits for d. Short sleeps run inline; longer sleeps defer
// the run via the server so the worker process can release.
func (s *StepAPI) Sleep(name string, d time.Duration) error {
	key := "__sleep:" + name
	s.mu.Lock()
	if stored, ok := s.state[key]; ok {
		s.mu.Unlock()
		if m, ok := stored.(map[string]any); ok {
			if wakeMs, ok := m["wakeAt"].(float64); ok {
				wakeAt := time.UnixMilli(int64(wakeMs))
				if time.Now().Before(wakeAt) {
					return &deferSignal{wakeAt: &wakeAt}
				}
				s.mu.Lock()
				if _, hasName := s.state[name]; !hasName {
					s.state[name] = true
					s.mu.Unlock()
					return s.client.SaveStep(s.ctx, s.runID, s.workerID, name, true)
				}
				s.mu.Unlock()
				return nil
			}
		}
		return nil
	}
	s.mu.Unlock()

	wakeAt := time.Now().Add(d)
	wakeMs := wakeAt.UnixMilli()
	marker := map[string]any{"wakeAt": wakeMs}
	s.mu.Lock()
	s.state[key] = marker
	s.mu.Unlock()
	if err := s.client.SaveStep(s.ctx, s.runID, s.workerID, key, marker); err != nil {
		return err
	}
	if d < inlineSleepThreshold {
		time.Sleep(d)
		s.mu.Lock()
		s.state[name] = true
		s.mu.Unlock()
		return s.client.SaveStep(s.ctx, s.runID, s.workerID, name, true)
	}
	return &deferSignal{wakeAt: &wakeAt}
}

// WaitFor parks the run waiting for a Signal with the given name. timeout
// of 0 means "no timeout" (parked indefinitely until matching signal). On
// resume, out (a *T) is populated with the signal's payload.
func (s *StepAPI) WaitFor(name string, out any, timeout time.Duration) error {
	s.mu.Lock()
	cached, ok := s.state[name]
	s.mu.Unlock()
	if ok {
		return assignCached(cached, out)
	}
	var timeoutAt *time.Time
	if timeout > 0 {
		t := time.Now().Add(timeout)
		timeoutAt = &t
	}
	return &waitSignal{stepName: name, eventName: name, timeoutAt: timeoutAt}
}

func assignCached(cached, out any) error {
	if out == nil {
		return nil
	}
	bs, err := json.Marshal(cached)
	if err != nil {
		return err
	}
	return json.Unmarshal(bs, out)
}

type workflowRegistration struct {
	handler Handler
	config  workflowConfig
}

type workflowConfig struct {
	maxAttempts uint32
	timeoutMs   uint32
	schedule    string
	backoffBase time.Duration
	backoffMax  time.Duration
}

// WorkflowOption configures a registered workflow.
type WorkflowOption func(*workflowConfig)

// WithMaxAttempts sets the total run-level attempt budget.
func WithMaxAttempts(n uint32) WorkflowOption {
	return func(c *workflowConfig) { c.maxAttempts = n }
}

// WithSchedule registers a cron schedule for this workflow.
func WithSchedule(expr string) WorkflowOption {
	return func(c *workflowConfig) { c.schedule = expr }
}

// WithBackoff sets run-level backoff between failed attempts.
func WithBackoff(base, max time.Duration) WorkflowOption {
	return func(c *workflowConfig) {
		c.backoffBase = base
		c.backoffMax = max
	}
}

var (
	registryMu      sync.Mutex
	registry        = map[string]workflowRegistration{}
	registryStarted bool
)

// RegisterWorkflow registers a handler under `name`. Must be called before
// RunWorker.
func RegisterWorkflow(name string, h Handler, opts ...WorkflowOption) {
	registryMu.Lock()
	defer registryMu.Unlock()
	if registryStarted {
		panic(fmt.Sprintf("tako: RegisterWorkflow(%q) called after RunWorker started", name))
	}
	if _, dup := registry[name]; dup {
		panic(fmt.Sprintf("tako: workflow %q already registered", name))
	}
	var cfg workflowConfig
	for _, opt := range opts {
		opt(&cfg)
	}
	registry[name] = workflowRegistration{handler: h, config: cfg}
}

// RunWorker connects to the enqueue socket, registers schedules, and runs
// the claim loop until ctx is cancelled or the idle timeout fires.
func RunWorker(ctx context.Context) error {
	registryMu.Lock()
	if len(registry) == 0 {
		registryMu.Unlock()
		return errors.New("tako: no workflows registered")
	}
	registryStarted = true
	handlers := make(map[string]workflowRegistration, len(registry))
	for k, v := range registry {
		handlers[k] = v
	}
	registryMu.Unlock()

	client, err := ClientFromEnv()
	if err != nil {
		return err
	}

	var schedules []ScheduleSpec
	for name, reg := range handlers {
		if reg.config.schedule != "" {
			schedules = append(schedules, ScheduleSpec{Name: name, Cron: reg.config.schedule})
		}
	}
	if len(schedules) > 0 {
		if err := client.RegisterSchedules(ctx, schedules); err != nil {
			return fmt.Errorf("register schedules: %w", err)
		}
	}

	w := newWorker(ctx, client, handlers)
	return w.run()
}

type worker struct {
	ctx         context.Context
	client      *Client
	handlers    map[string]workflowRegistration
	workerID    string
	leaseMs     uint64
	heartbeatMs uint64
	pollMs      int
	baseBackoff time.Duration
	maxBackoff  time.Duration
	idleTimeout time.Duration
	lastClaimAt atomic.Int64
}

func newWorker(ctx context.Context, client *Client, handlers map[string]workflowRegistration) *worker {
	w := &worker{
		ctx:         ctx,
		client:      client,
		handlers:    handlers,
		workerID:    fmt.Sprintf("worker-%d", os.Getpid()),
		leaseMs:     60_000,
		heartbeatMs: 20_000,
		pollMs:      1_000,
		baseBackoff: time.Second,
		maxBackoff:  time.Hour,
		idleTimeout: parseIdleTimeout(),
	}
	w.lastClaimAt.Store(time.Now().UnixMilli())
	return w
}

func (w *worker) run() error {
	names := make([]string, 0, len(w.handlers))
	for name := range w.handlers {
		names = append(names, name)
	}

	for {
		select {
		case <-w.ctx.Done():
			return nil
		default:
		}

		task, err := w.client.Claim(w.ctx, w.workerID, names, w.leaseMs)
		if err != nil {
			fmt.Fprintln(os.Stderr, "tako-worker: claim error:", err)
			time.Sleep(time.Duration(w.pollMs) * time.Millisecond)
			continue
		}
		if task == nil {
			if w.idleTimeout > 0 &&
				time.Since(time.UnixMilli(w.lastClaimAt.Load())) >= w.idleTimeout {
				return nil
			}
			select {
			case <-w.ctx.Done():
				return nil
			case <-time.After(time.Duration(w.pollMs) * time.Millisecond):
			}
			continue
		}

		w.lastClaimAt.Store(time.Now().UnixMilli())
		w.execute(task)
	}
}

func (w *worker) execute(task *Run) {
	reg, ok := w.handlers[task.Name]
	if !ok {
		_ = w.client.Fail(w.ctx, task.ID, w.workerID, fmt.Sprintf("no handler registered for %q", task.Name), nil, true)
		return
	}

	stepState := task.StepState
	if stepState == nil {
		stepState = map[string]any{}
	}
	ctx := &WorkflowContext{
		RunID:        task.ID,
		WorkflowName: task.Name,
		Attempts:     task.Attempts,
		Step: &StepAPI{
			client:   w.client,
			ctx:      w.ctx,
			runID:    task.ID,
			workerID: w.workerID,
			state:    stepState,
		},
		ctx: w.ctx,
	}

	hbStop := make(chan struct{})
	go w.heartbeatLoop(task.ID, hbStop)
	defer close(hbStop)

	err := reg.handler(ctx, task.Payload)
	if err != nil {
		var bs *bailSignal
		var fs *failSignal
		var ds *deferSignal
		var ws *waitSignal
		switch {
		case errors.As(err, &bs):
			reason := bs.reason
			var rp *string
			if reason != "" {
				rp = &reason
			}
			_ = w.client.Cancel(w.ctx, task.ID, w.workerID, rp)
		case errors.As(err, &fs):
			_ = w.client.Fail(w.ctx, task.ID, w.workerID, fs.err.Error(), nil, true)
		case errors.As(err, &ds):
			_ = w.client.Defer(w.ctx, task.ID, w.workerID, ds.wakeAt)
		case errors.As(err, &ws):
			_ = w.client.WaitForEvent(w.ctx, task.ID, w.workerID, ws.stepName, ws.eventName, ws.timeoutAt)
		default:
			maxAttempts := reg.config.maxAttempts
			if maxAttempts == 0 {
				maxAttempts = task.MaxAttempts
			}
			finalize := task.Attempts >= maxAttempts
			var next *time.Time
			if !finalize {
				base := reg.config.backoffBase
				if base == 0 {
					base = w.baseBackoff
				}
				max := reg.config.backoffMax
				if max == 0 {
					max = w.maxBackoff
				}
				t := time.Now().Add(expBackoff(task.Attempts, base, max))
				next = &t
			}
			_ = w.client.Fail(w.ctx, task.ID, w.workerID, err.Error(), next, finalize)
		}
		return
	}
	_ = w.client.Complete(w.ctx, task.ID, w.workerID)
}

func (w *worker) heartbeatLoop(id string, stop <-chan struct{}) {
	ticker := time.NewTicker(time.Duration(w.heartbeatMs) * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-stop:
			return
		case <-ticker.C:
			_ = w.client.Heartbeat(w.ctx, id, w.workerID, w.leaseMs)
		}
	}
}

func expBackoff(attempts uint32, base, max time.Duration) time.Duration {
	if attempts < 1 {
		attempts = 1
	}
	exp := base * time.Duration(math.Pow(2, float64(attempts-1)))
	if exp > max {
		exp = max
	}
	jitter := time.Duration(float64(exp) * 0.2 * (rand.Float64()*2 - 1))
	result := exp + jitter
	if result < 0 {
		result = 0
	}
	return result
}

func parseIdleTimeout() time.Duration {
	raw := os.Getenv("TAKO_WORKER_IDLE_TIMEOUT_MS")
	if raw == "" {
		return 0
	}
	var ms int64
	_, err := fmt.Sscanf(raw, "%d", &ms)
	if err != nil || ms < 0 {
		return 0
	}
	return time.Duration(ms) * time.Millisecond
}
