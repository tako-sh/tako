package tako

import (
	"context"
	cryptorand "crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"math/rand"
	"os"
	"strconv"
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

	resultMu sync.Mutex
	result   json.RawMessage
}

// Done returns the context's cancellation channel.
func (c *WorkflowContext) Done() <-chan struct{} { return c.ctx.Done() }

// SetResult records value as the run's result. It is saved when the handler
// returns nil and callers read it with Result. The last call wins; a run
// that never calls SetResult completes with a null result. Values that do
// not JSON-encode are rejected and leave any earlier result in place.
func (c *WorkflowContext) SetResult(value any) error {
	encoded, err := json.Marshal(value)
	if err != nil {
		return fmt.Errorf("tako: workflow result is not JSON-encodable: %w", err)
	}
	c.resultMu.Lock()
	c.result = encoded
	c.resultMu.Unlock()
	return nil
}

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

// errOwnershipLost stops an attempt whose run is no longer owned by this
// worker. Every step is a checkpoint that returns it once ownership is lost.
var errOwnershipLost = &staleOwnerError{message: "tako: this worker no longer owns the run"}

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
	// used holds the step names already taken in this execution.
	used map[string]struct{}
	mu   sync.Mutex
	// lost is set once a write or heartbeat is rejected as stale.
	lost *atomic.Bool
}

func (s *StepAPI) ownershipLost() bool {
	return s.lost != nil && s.lost.Load()
}

// once reserves name for one step in this execution of the handler. Run,
// Sleep, and WaitFor share one namespace of saved steps, so a second use
// would silently return the first use's saved result instead of doing its
// own work. Reuse is a programming error that no retry can fix, so it fails
// the run immediately. A step that returns an error releases its name:
// nothing was saved, and the handler may try the same step again.
func (s *StepAPI) once(name string, step func() error) error {
	if s.ownershipLost() {
		return errOwnershipLost
	}
	s.mu.Lock()
	if _, reused := s.used[name]; reused {
		s.mu.Unlock()
		return &failSignal{err: fmt.Errorf(
			"step name %q was used more than once in this run; step names must be unique within a run, so add an index or id to the name (for example %q)",
			name, name+":1")}
	}
	if s.used == nil {
		s.used = map[string]struct{}{}
	}
	s.used[name] = struct{}{}
	s.mu.Unlock()

	err := step()
	if err != nil {
		s.mu.Lock()
		delete(s.used, name)
		s.mu.Unlock()
	}
	return err
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
//
// Step names must be unique within a run. Using a name twice in one
// execution, across Run, Sleep, and WaitFor, fails the run.
func (s *StepAPI) Run(name string, out any, fn func() (any, error), opts ...StepRunOpts) error {
	return s.once(name, func() error { return s.run(name, out, fn, opts...) })
}

func (s *StepAPI) run(name string, out any, fn func() (any, error), opts ...StepRunOpts) error {
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
			if perr := s.saveStep(name, value); perr != nil {
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

// Cache only acknowledged checkpoints. A caller may catch a persistence error
// and retry the same step without ending this execution.
func (s *StepAPI) saveStep(name string, value any) error {
	if err := s.client.SaveStep(s.ctx, s.runID, s.workerID, name, value); err != nil {
		if isStaleOwner(err) && s.lost != nil {
			s.lost.Store(true)
		}
		return err
	}
	s.mu.Lock()
	s.state[name] = value
	s.mu.Unlock()
	return nil
}

func isControlSignal(err error) bool {
	var bs *bailSignal
	var fs *failSignal
	var ds *deferSignal
	var ws *waitSignal
	return errors.As(err, &bs) || errors.As(err, &fs) ||
		errors.As(err, &ds) || errors.As(err, &ws) || isStaleOwner(err)
}

// Sleep durably waits for d. Short sleeps run inline; longer sleeps defer
// the run via the server so the worker process can release.
func (s *StepAPI) Sleep(name string, d time.Duration) error {
	return s.once(name, func() error { return s.sleep(name, d) })
}

func (s *StepAPI) sleep(name string, d time.Duration) error {
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
					s.mu.Unlock()
					return s.saveStep(name, true)
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
	// Match the representation restored from JSON on a subsequent claim.
	marker := map[string]any{"wakeAt": float64(wakeMs)}
	if err := s.saveStep(key, marker); err != nil {
		return err
	}
	if d < inlineSleepThreshold {
		time.Sleep(d)
		return s.saveStep(name, true)
	}
	return &deferSignal{wakeAt: &wakeAt}
}

// WaitFor parks the run waiting for a Signal with the given name. timeout
// of 0 means "no timeout" (parked indefinitely until matching signal). On
// resume, out (a *T) is populated with the signal's payload.
func (s *StepAPI) WaitFor(name string, out any, timeout time.Duration) error {
	return s.once(name, func() error { return s.waitFor(name, out, timeout) })
}

func (s *StepAPI) waitFor(name string, out any, timeout time.Duration) error {
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
	schedule    string
	backoffBase time.Duration
	backoffMax  time.Duration
}

// WorkflowOption configures a registered workflow.
type WorkflowOption func(*workflowConfig)

// WithMaxAttempts sets the total run-level attempt budget that Enqueue
// stores on new runs of this workflow when EnqueueOpts.MaxAttempts is nil.
// The worker always honors the budget stored on the run, so runs enqueued
// by a process that did not register the workflow, and runs created by a
// schedule, keep their own stored budget (the server default is 3).
func WithMaxAttempts(n uint32) WorkflowOption {
	return func(c *workflowConfig) { c.maxAttempts = n }
}

func registeredMaxAttempts(name string) uint32 {
	registryMu.Lock()
	defer registryMu.Unlock()
	return registry[name].config.maxAttempts
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
// the claim loop until ctx is cancelled or the idle timeout fires. Cancellation
// stops new claims and drains active handlers with their leases still renewed.
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
	runCtx      context.Context
	concurrency int
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

	lifecycleRetryDelays []time.Duration
}

func newWorker(ctx context.Context, client *Client, handlers map[string]workflowRegistration) *worker {
	var processID [16]byte
	if _, err := cryptorand.Read(processID[:]); err != nil {
		panic(fmt.Sprintf("generate workflow worker identity: %v", err))
	}
	w := &worker{
		ctx:         ctx,
		runCtx:      context.WithoutCancel(ctx),
		concurrency: parseWorkerConcurrency(),
		client:      client,
		handlers:    handlers,
		workerID:    fmt.Sprintf("worker-%d-%x", os.Getpid(), processID),
		leaseMs:     60_000,
		heartbeatMs: 20_000,
		pollMs:      1_000,
		baseBackoff: time.Second,
		maxBackoff:  time.Hour,
		idleTimeout: parseIdleTimeout(),

		lifecycleRetryDelays: defaultLifecycleRetryDelays,
	}
	w.lastClaimAt.Store(time.Now().UnixMilli())
	return w
}

func (w *worker) run() error {
	var active sync.WaitGroup
	defer active.Wait()
	slots := make(chan struct{}, w.concurrency)
	names := make([]string, 0, len(w.handlers))
	for name := range w.handlers {
		names = append(names, name)
	}

	for {
		select {
		case <-w.ctx.Done():
			return nil
		case slots <- struct{}{}:
		}
		if w.ctx.Err() != nil {
			<-slots
			return nil
		}

		task, err := w.client.Claim(w.ctx, w.workerID, names, w.leaseMs)
		if err != nil {
			<-slots
			if w.ctx.Err() != nil {
				return nil
			}
			fmt.Fprintln(os.Stderr, "tako-worker: claim error:", err)
			select {
			case <-w.ctx.Done():
				return nil
			case <-time.After(time.Duration(w.pollMs) * time.Millisecond):
			}
			continue
		}
		if task == nil {
			<-slots
			if len(slots) == 0 && w.idleTimeout > 0 &&
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
		active.Add(1)
		go func() {
			defer active.Done()
			defer func() {
				w.lastClaimAt.Store(time.Now().UnixMilli())
				<-slots
			}()
			w.execute(task)
		}()
	}
}

// defaultLifecycleRetryDelays space retries of a run's final write after a
// transient failure such as a socket error or a storage timeout.
var defaultLifecycleRetryDelays = []time.Duration{
	100 * time.Millisecond, 250 * time.Millisecond, 500 * time.Millisecond, time.Second,
}

// finalize writes a run's outcome, retrying transient failures while the
// heartbeat keeps the lease. A run left running is reclaimed after its lease
// expires, and a lost last attempt ends the run as dead although its handler
// never failed. Retries stop once ownership is lost.
func (w *worker) finalize(op, runID string, lost *atomic.Bool, write func() error) {
	err := write()
	for _, delay := range w.lifecycleRetryDelays {
		if err == nil || isStaleOwner(err) || lost.Load() {
			break
		}
		fmt.Fprintf(os.Stderr, "tako-worker: %s failed for run %s, retrying: %v\n", op, runID, err)
		time.Sleep(delay)
		err = write()
	}
	switch {
	case err == nil:
	case isStaleOwner(err) || lost.Load():
		stopped(runID)
	default:
		// The run re-executes after lease expiry; leave a forensic trail.
		fmt.Fprintf(os.Stderr, "tako-worker: %s failed for run %s: %v\n", op, runID, err)
	}
}

// stopped reports an attempt that lost its run to an operator cancel or a
// reclaim. Another owner or the operator decides the run's fate.
func stopped(runID string) {
	fmt.Fprintf(os.Stderr, "tako-worker: run %s stopped: this worker no longer owns it\n", runID)
}

func (w *worker) execute(task *Run) {
	if task.WorkerID == "" {
		fmt.Fprintf(os.Stderr, "tako-worker: claim failed for run %s: claim response is missing its ownership identifier\n", task.ID)
		return
	}
	lost := &atomic.Bool{}
	reg, ok := w.handlers[task.Name]
	if !ok {
		w.finalize("fail", task.ID, lost, func() error {
			return w.client.Fail(w.runCtx, task.ID, task.WorkerID, fmt.Sprintf("no handler registered for %q", task.Name), nil, true)
		})
		return
	}

	stepState := task.StepState
	if stepState == nil {
		stepState = map[string]any{}
	}
	// Done fires when ownership is lost, so handlers can stop before their
	// next checkpoint. RPCs keep the uncancelled context.
	handlerCtx, cancelHandler := context.WithCancel(w.runCtx)
	defer cancelHandler()
	ctx := &WorkflowContext{
		RunID:        task.ID,
		WorkflowName: task.Name,
		Attempts:     task.Attempts,
		Step: &StepAPI{
			client:   w.client,
			ctx:      w.runCtx,
			runID:    task.ID,
			workerID: task.WorkerID,
			state:    stepState,
			lost:     lost,
		},
		ctx: handlerCtx,
	}

	hbStop := make(chan struct{})
	go w.heartbeatLoop(task.ID, task.WorkerID, hbStop, func() {
		lost.Store(true)
		cancelHandler()
	})
	defer close(hbStop)

	err := reg.handler(ctx, task.Payload)
	// Never fail, retry, or complete a run this attempt no longer owns.
	if lost.Load() || isStaleOwner(err) {
		stopped(task.ID)
		return
	}
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
			w.finalize("cancel", task.ID, lost, func() error {
				return w.client.Cancel(w.runCtx, task.ID, task.WorkerID, rp)
			})
		case errors.As(err, &fs):
			w.finalize("fail", task.ID, lost, func() error {
				return w.client.Fail(w.runCtx, task.ID, task.WorkerID, fs.err.Error(), nil, true)
			})
		case errors.As(err, &ds):
			w.finalize("defer", task.ID, lost, func() error {
				return w.client.Defer(w.runCtx, task.ID, task.WorkerID, ds.wakeAt)
			})
		case errors.As(err, &ws):
			w.finalize("wait-for-event", task.ID, lost, func() error {
				return w.client.WaitForEvent(w.runCtx, task.ID, task.WorkerID, ws.stepName, ws.eventName, ws.timeoutAt)
			})
		default:
			// The budget stored on the run is authoritative: it carries
			// per-enqueue overrides and is what the server enforces when
			// it reclaims a lost worker's run.
			finalize := task.Attempts >= task.MaxAttempts
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
			w.finalize("fail", task.ID, lost, func() error {
				return w.client.Fail(w.runCtx, task.ID, task.WorkerID, err.Error(), next, finalize)
			})
		}
		return
	}
	ctx.resultMu.Lock()
	result := ctx.result
	ctx.resultMu.Unlock()
	w.finalize("complete", task.ID, lost, func() error {
		return w.client.Complete(w.runCtx, task.ID, task.WorkerID, result)
	})
}

func (w *worker) heartbeatLoop(id, claimOwner string, stop <-chan struct{}, onLost func()) {
	ticker := time.NewTicker(time.Duration(w.heartbeatMs) * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-stop:
			return
		case <-ticker.C:
			if err := w.client.Heartbeat(w.runCtx, id, claimOwner, w.leaseMs); isStaleOwner(err) {
				onLost()
				return
			}
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

func parseWorkerConcurrency() int {
	value, err := strconv.Atoi(os.Getenv("TAKO_WORKER_CONCURRENCY"))
	if err != nil || value < 1 {
		return 500
	}
	return value
}
