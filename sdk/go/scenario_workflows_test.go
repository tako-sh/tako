package tako

import (
	"encoding/json"
	"errors"
	"time"
)

// scenarioWorkflow is one test workflow of the shared workflow scenarios in
// test-support/workflow-scenarios. The JavaScript runner defines the same
// workflows under the same names with the same behavior; the scenarios'
// README describes them.
type scenarioWorkflow struct {
	handler Handler
	opts    []WorkflowOption
}

var scenarioWorkflows = map[string]scenarioWorkflow{
	"steps": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		var a string
		if err := ctx.Step.Run("a", &a, func() (any, error) { return "a-done", nil }); err != nil {
			return err
		}
		var b int
		if err := ctx.Step.Run("b", &b, func() (any, error) { return 2, nil }); err != nil {
			return err
		}
		return ctx.SetResult(map[string]any{"a": a, "b": b})
	}},
	"fails": {handler: func(*WorkflowContext, json.RawMessage) error {
		return errors.New("boom")
	}},
	"sleeps": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		if err := ctx.Step.Sleep("nap", time.Hour); err != nil {
			return err
		}
		return ctx.SetResult("awake")
	}},
	"wait": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		var event any
		if err := ctx.Step.WaitFor("approval", &event, 0); err != nil {
			return err
		}
		return ctx.SetResult(event)
	}},
	"wait-timeout": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		var event any
		if err := ctx.Step.WaitFor("approval", &event, 10*time.Minute); err != nil {
			return err
		}
		return ctx.SetResult(event)
	}},
	"duplicate": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		if err := ctx.Step.Run("a", nil, func() (any, error) { return 1, nil }); err != nil {
			return err
		}
		return ctx.Step.Run("a", nil, func() (any, error) { return 2, nil })
	}},
	"bail": {handler: func(ctx *WorkflowContext, _ json.RawMessage) error {
		return ctx.Bail("not needed")
	}},
	"nightly": {
		handler: func(*WorkflowContext, json.RawMessage) error { return nil },
		opts:    []WorkflowOption{WithSchedule("0 3 * * *"), WithMaxAttempts(5)},
	},
	"hourly": {
		handler: func(*WorkflowContext, json.RawMessage) error { return nil },
		opts:    []WorkflowOption{WithSchedule("0 * * * *")},
	},
}
