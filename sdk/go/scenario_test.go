package tako

// Plays every shared workflow scenario in test-support/workflow-scenarios
// against the real Go worker. The JavaScript SDK plays the same files; the
// README in that directory defines the format and the matching rules, which
// this file implements. go:embed cannot reach outside the module, so the
// files are read at test time by relative path.

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
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

const (
	scenarioDir = "../../test-support/workflow-scenarios"
	scenarioApp = "scenario-app"
	// Far longer than any scenario, so a heartbeat is never due.
	scenarioHeartbeatMs = 3_600_000
)

type scenarioStep struct {
	Expect any `json:"expect"`
	Reply  any `json:"reply"`
}

type scenario struct {
	Description string         `json:"description"`
	Drive       string         `json:"drive"`
	Workflows   []string       `json:"workflows"`
	Steps       []scenarioStep `json:"steps"`
}

func loadScenario(t *testing.T, path string) scenario {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read scenario: %v", err)
	}
	dec := json.NewDecoder(strings.NewReader(string(data)))
	dec.DisallowUnknownFields()
	var s scenario
	if err := dec.Decode(&s); err != nil {
		t.Fatalf("parse scenario: %v", err)
	}
	if s.Drive != "run" && s.Drive != "start" {
		t.Fatalf("unknown drive %q", s.Drive)
	}
	if len(s.Workflows) == 0 || len(s.Steps) == 0 {
		t.Fatalf("a scenario needs workflows and steps")
	}
	for _, name := range s.Workflows {
		if _, ok := scenarioWorkflows[name]; !ok {
			t.Fatalf("unknown test workflow %q", name)
		}
	}
	var raw struct {
		Steps []map[string]json.RawMessage `json:"steps"`
	}
	_ = json.Unmarshal(data, &raw)
	for i, step := range raw.Steps {
		if _, ok := step["expect"]; !ok || len(step) != 2 {
			t.Fatalf("step %d must have exactly \"expect\" and \"reply\"", i)
		}
		if _, ok := step["reply"]; !ok {
			t.Fatalf("step %d must have exactly \"expect\" and \"reply\"", i)
		}
	}
	return s
}

// --- Matching (rules: test-support/workflow-scenarios/README.md) ---

type mismatch struct{ msg string }

func (m *mismatch) Error() string { return m.msg }

var scenarioOperators = map[string][]string{
	"$var":         {"$var", "is"},
	"$any":         {"$any"},
	"$type":        {"$type"},
	"$positive":    {"$positive"},
	"$contains":    {"$contains"},
	"$ms_from_now": {"$ms_from_now"},
	"$unordered":   {"$unordered"},
}

func jsonType(v any) string {
	switch v.(type) {
	case nil:
		return "null"
	case []any:
		return "array"
	case map[string]any:
		return "object"
	case string:
		return "string"
	case float64:
		return "number"
	case bool:
		return "boolean"
	}
	return fmt.Sprintf("%T", v)
}

// operatorOf returns the matcher an object names, "" for a plain object, or
// an error for a matcher this runner does not know.
func operatorOf(expected map[string]any, path string) (string, error) {
	var dollar []string
	for key := range expected {
		if strings.HasPrefix(key, "$") {
			dollar = append(dollar, key)
		}
	}
	if len(dollar) == 0 {
		return "", nil
	}
	op := dollar[0]
	allowed, known := scenarioOperators[op]
	ok := len(dollar) == 1 && known && (op == "$var" || len(expected) == 1)
	for key := range expected {
		if !contains(allowed, key) {
			ok = false
		}
	}
	if !ok {
		encoded, _ := json.Marshal(expected)
		return "", fmt.Errorf("%s: unknown matcher %s", path, encoded)
	}
	return op, nil
}

func scenarioMatch(expected, actual any, path string, vars map[string]any, now int64) error {
	fail := func(why string) error {
		e, _ := json.Marshal(expected)
		a, _ := json.Marshal(actual)
		return &mismatch{fmt.Sprintf("%s: %s; expected %s, got %s", path, why, e, a)}
	}
	if items, ok := expected.([]any); ok {
		got, ok := actual.([]any)
		if !ok || len(got) != len(items) {
			return fail("array differs")
		}
		for i := range items {
			if err := scenarioMatch(items[i], got[i], fmt.Sprintf("%s[%d]", path, i), vars, now); err != nil {
				return err
			}
		}
		return nil
	}
	object, ok := expected.(map[string]any)
	if !ok {
		if !reflect.DeepEqual(expected, actual) {
			return fail("value differs")
		}
		return nil
	}
	op, err := operatorOf(object, path)
	if err != nil {
		return err
	}
	switch op {
	case "":
		got, ok := actual.(map[string]any)
		if !ok {
			return fail("not an object")
		}
		keys := make([]string, 0, len(object))
		for key := range object {
			keys = append(keys, key)
		}
		sort.Strings(keys)
		for _, key := range keys {
			value, ok := got[key]
			if !ok {
				return fail(fmt.Sprintf("missing field %q", key))
			}
			if err := scenarioMatch(object[key], value, path+"."+key, vars, now); err != nil {
				return err
			}
		}
		for key := range got {
			if _, ok := object[key]; !ok {
				return fail(fmt.Sprintf("unexpected field %q", key))
			}
		}
		return nil
	case "$any":
		return nil
	case "$type":
		if jsonType(actual) != object["$type"] {
			return fail("wrong type")
		}
		return nil
	case "$positive":
		if n, ok := actual.(float64); !ok || !(n > 0) {
			return fail("not a positive number")
		}
		return nil
	case "$contains":
		text, ok := actual.(string)
		want, _ := object["$contains"].(string)
		if !ok || !strings.Contains(text, want) {
			return fail("text not contained")
		}
		return nil
	case "$ms_from_now":
		bounds, _ := object["$ms_from_now"].([]any)
		if len(bounds) != 2 {
			return fmt.Errorf("%s: $ms_from_now needs [min, max]", path)
		}
		min, _ := bounds[0].(float64)
		max, _ := bounds[1].(float64)
		n, ok := actual.(float64)
		if !ok {
			return fail("not a number")
		}
		delta := n - float64(now)
		if delta < min || delta > max {
			return fail(fmt.Sprintf("%v ms from now is outside [%v, %v]", delta, min, max))
		}
		return nil
	case "$var":
		name := fmt.Sprint(object["$var"])
		if bound, ok := vars[name]; ok {
			if !reflect.DeepEqual(bound, actual) {
				b, _ := json.Marshal(bound)
				return fail(fmt.Sprintf("differs from %s = %s", name, b))
			}
			return nil
		}
		if is, ok := object["is"]; ok {
			if err := scenarioMatch(is, actual, path, vars, now); err != nil {
				return err
			}
		} else if actual == nil {
			return fail("missing value")
		}
		vars[name] = actual
		return nil
	case "$unordered":
		items, _ := object["$unordered"].([]any)
		got, ok := actual.([]any)
		if !ok || len(got) != len(items) {
			return fail("array differs")
		}
		var assign func(i int, free []any, current map[string]any) (map[string]any, error)
		assign = func(i int, free []any, current map[string]any) (map[string]any, error) {
			if i == len(items) {
				return current, nil
			}
			for j, candidate := range free {
				trial := make(map[string]any, len(current))
				for k, v := range current {
					trial[k] = v
				}
				err := scenarioMatch(items[i], candidate, fmt.Sprintf("%s{%d}", path, i), trial, now)
				if _, isMismatch := err.(*mismatch); isMismatch {
					continue
				}
				if err != nil {
					return nil, err
				}
				rest := append(append([]any{}, free[:j]...), free[j+1:]...)
				done, err := assign(i+1, rest, trial)
				if err != nil || done != nil {
					return done, err
				}
			}
			return nil, nil
		}
		done, err := assign(0, got, vars)
		if err != nil {
			return err
		}
		if done == nil {
			return fail("no ordering matches")
		}
		for k, v := range done {
			vars[k] = v
		}
		return nil
	}
	return fmt.Errorf("%s: unhandled matcher %s", path, op)
}

// --- Fake internal socket ---

type scenarioSocket struct {
	steps []scenarioStep
	// finished is called once, when the last step matched or a request failed.
	finished func()

	mu       sync.Mutex
	next     int
	vars     map[string]any
	failures []string
	once     sync.Once
}

func startScenarioSocket(t *testing.T, steps []scenarioStep, finished func()) (*scenarioSocket, string) {
	t.Helper()
	dir, err := os.MkdirTemp("/tmp", "tako-scenario-")
	if err != nil {
		t.Fatalf("tempdir: %v", err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(dir) })
	path := filepath.Join(dir, "internal.sock")
	l, err := net.Listen("unix", path)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { _ = l.Close() })
	s := &scenarioSocket{steps: steps, finished: finished, vars: map[string]any{}}
	go func() {
		for {
			conn, err := l.Accept()
			if err != nil {
				return
			}
			go s.serve(conn)
		}
	}()
	return s, path
}

func (s *scenarioSocket) serve(conn net.Conn) {
	defer conn.Close()
	r := bufio.NewReader(conn)
	for {
		line, err := r.ReadBytes('\n')
		if err != nil {
			return
		}
		var request any
		if err := json.Unmarshal(line, &request); err != nil {
			s.fail(fmt.Sprintf("request is not JSON: %q", line))
			return
		}
		out, _ := json.Marshal(s.answer(request))
		if _, err := conn.Write(append(out, '\n')); err != nil {
			return
		}
	}
}

func (s *scenarioSocket) fail(msg string) {
	s.mu.Lock()
	s.failures = append(s.failures, msg)
	s.next = len(s.steps)
	s.mu.Unlock()
	s.once.Do(s.finished)
}

func (s *scenarioSocket) answer(request any) any {
	now := time.Now().UnixMilli()
	s.mu.Lock()
	index := s.next
	if index >= len(s.steps) {
		s.mu.Unlock()
		encoded, _ := json.Marshal(request)
		s.fail("unexpected request after the last step: " + string(encoded))
		return map[string]any{"status": "error", "message": "scenario: unexpected request"}
	}
	step := s.steps[index]
	if err := scenarioMatch(step.Expect, request, fmt.Sprintf("step %d", index), s.vars, now); err != nil {
		s.mu.Unlock()
		s.fail(err.Error())
		return map[string]any{"status": "error", "message": "scenario: request did not match"}
	}
	s.next++
	last := s.next == len(s.steps)
	s.mu.Unlock()
	if last {
		s.once.Do(s.finished)
	}
	return step.Reply
}

func (s *scenarioSocket) check(t *testing.T) {
	t.Helper()
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, failure := range s.failures {
		t.Error(failure)
	}
	if len(s.failures) == 0 && s.next != len(s.steps) {
		t.Errorf("the worker stopped after %d of %d steps", s.next, len(s.steps))
	}
}

// --- Drives ---

// registerScenarioWorkflows registers the named test workflows the way an
// app does and returns the handlers RunWorker would take from the registry.
func registerScenarioWorkflows(names []string) map[string]workflowRegistration {
	resetRegistry()
	for _, name := range names {
		w := scenarioWorkflows[name]
		RegisterWorkflow(name, w.handler, w.opts...)
	}
	registryMu.Lock()
	defer registryMu.Unlock()
	handlers := make(map[string]workflowRegistration, len(registry))
	for k, v := range registry {
		handlers[k] = v
	}
	return handlers
}

// driveScenarioRun claims once and executes the claimed run: the two calls
// the worker's claim loop makes for each run.
func driveScenarioRun(s scenario, path string) {
	handlers := registerScenarioWorkflows(s.Workflows)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	w := newWorker(ctx, NewClient(path, scenarioApp), handlers)
	w.heartbeatMs = scenarioHeartbeatMs
	names := make([]string, 0, len(handlers))
	for name := range handlers {
		names = append(names, name)
	}
	task, err := w.client.Claim(w.ctx, w.workerID, names, w.leaseMs)
	if err != nil || task == nil {
		return
	}
	w.execute(task)
}

// driveScenarioStart starts the worker as an app's main does and stops it
// before its claim loop: start scenarios cover what precedes the first claim.
func driveScenarioStart(ctx context.Context, t *testing.T, s scenario, path string) {
	registerScenarioWorkflows(s.Workflows)
	t.Setenv(WorkflowSocketEnv, path)
	t.Setenv(AppNameEnv, scenarioApp)
	if err := RunWorker(ctx); err != nil {
		t.Errorf("run worker: %v", err)
	}
}

func TestWorkflowScenarios(t *testing.T) {
	entries, err := os.ReadDir(scenarioDir)
	if err != nil {
		t.Fatalf("read the shared workflow scenarios at %s: %v", scenarioDir, err)
	}
	var files []string
	for _, entry := range entries {
		if strings.HasSuffix(entry.Name(), ".json") {
			files = append(files, entry.Name())
		}
	}
	if len(files) == 0 {
		t.Fatalf("no workflow scenarios in %s", scenarioDir)
	}
	sort.Strings(files)
	defer resetRegistry()
	for _, file := range files {
		t.Run(strings.TrimSuffix(file, ".json"), func(t *testing.T) {
			s := loadScenario(t, filepath.Join(scenarioDir, file))
			ctx, stop := context.WithTimeout(context.Background(), 5*time.Second)
			defer stop()
			socket, path := startScenarioSocket(t, s.Steps, stop)
			if s.Drive == "run" {
				driveScenarioRun(s, path)
			} else {
				driveScenarioStart(ctx, t, s, path)
			}
			socket.check(t)
		})
	}
}
