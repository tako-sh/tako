package tako

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"time"
)

// NetworkEnqueueOptions overrides the recipient workflow's defaults.
type NetworkEnqueueOptions struct {
	RunAtMs     *int64  `json:"run_at_ms,omitempty"`
	MaxAttempts *uint32 `json:"max_attempts,omitempty"`
	ID          *string `json:"id,omitempty"`
}

type NetworkEnqueueResult struct {
	ID           string `json:"id"`
	Deduplicated bool   `json:"deduplicated"`
}

// Call executes a private network handler directly, without enqueueing or automatic retries.
func Call(ctx context.Context, address string, input any) (json.RawMessage, error) {
	return callNetwork(ctx, address, map[string]any{"operation": "call", "input": input})
}

// WorkflowResultError describes a terminal workflow outcome that has no result.
type WorkflowResultError struct {
	RunID   string
	Status  string
	Message string
}

func (e *WorkflowResultError) Error() string { return "tako: workflow " + e.Status + ": " + e.Message }

// Result waits for a retained workflow result. Cancelling ctx stops waiting, not the workflow.
func Result(ctx context.Context, address, id string) (json.RawMessage, error) {
	for {
		data, err := callNetwork(ctx, address, map[string]any{"operation": "result", "id": id})
		if err != nil {
			return nil, err
		}
		var state *struct {
			Status string          `json:"status"`
			Result json.RawMessage `json:"result"`
			Error  string          `json:"error"`
		}
		if err := json.Unmarshal(data, &state); err != nil {
			return nil, err
		}
		if state == nil {
			return nil, &WorkflowResultError{id, "missing", "run not found or no longer retained"}
		}
		switch state.Status {
		case "succeeded":
			return state.Result, nil
		case "dead", "cancelled":
			return nil, &WorkflowResultError{id, state.Status, state.Error}
		case "pending", "running":
		default:
			return nil, errors.New("tako: invalid workflow result response")
		}
		timer := time.NewTimer(time.Second)
		select {
		case <-ctx.Done():
			timer.Stop()
			return nil, ctx.Err()
		case <-timer.C:
		}
	}
}

// Enqueue starts a declared workflow by local name or app.workflow address.
func Enqueue(ctx context.Context, address string, payload any, options NetworkEnqueueOptions) (*NetworkEnqueueResult, error) {
	data, err := callNetwork(ctx, address, map[string]any{"operation": "enqueue", "payload": payload, "opts": options})
	if err != nil {
		return nil, err
	}
	var result NetworkEnqueueResult
	if err := json.Unmarshal(data, &result); err != nil {
		return nil, err
	}
	if result.ID == "" {
		return nil, errors.New("tako: invalid enqueue response")
	}
	return &result, nil
}

// Publish delivers a message to a channel owned by an app on this private network.
// params select the recipient channel's stream: only subscribers bound to the
// same flat string, number, or boolean params receive the message. Nil params
// select the unparameterized stream; a nil value inside params is rejected, so
// leave an unset param out. It acknowledges publication; it does not
// wait for subscribers to process it.
func Publish(ctx context.Context, address string, params any, messageType string, data any) (json.RawMessage, error) {
	if params == nil {
		params = map[string]any{}
	}
	return callNetwork(ctx, address, map[string]any{"operation": "publish", "params": params, "message": map[string]any{"type": messageType, "data": data}})
}

func callNetwork(ctx context.Context, address string, request any) (data json.RawMessage, err error) {
	socket, app := os.Getenv("TAKO_INTERNAL_SOCKET"), os.Getenv("TAKO_APP_NAME")
	if socket == "" || app == "" {
		return nil, errors.New("tako: network primitives require the Tako runtime")
	}
	body, err := json.Marshal(map[string]any{"command": "network_call", "app": app, "address": address, "request": request})
	if err != nil {
		return nil, err
	}
	const limit = 1024 * 1024
	if len(body) >= limit {
		return nil, errors.New("tako: network request too large")
	}
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	defer func() {
		// Cancellation closes the connection to interrupt in-flight I/O. Preserve
		// the context error instead of exposing that implementation's socket error.
		if err != nil && ctx.Err() != nil {
			err = ctx.Err()
		}
	}()
	conn, err := (&net.Dialer{}).DialContext(ctx, "unix", socket)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	stop := context.AfterFunc(ctx, func() { _ = conn.Close() })
	defer stop()
	if _, err := io.Copy(conn, bytes.NewReader(append(body, '\n'))); err != nil {
		return nil, err
	}
	line, err := bufio.NewReader(io.LimitReader(conn, limit+1)).ReadBytes('\n')
	if err != nil {
		return nil, err
	}
	if len(line) > limit {
		return nil, errors.New("tako: network response too large")
	}
	var response struct {
		Status string          `json:"status"`
		Data   json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(line, &response); err != nil {
		return nil, err
	}
	if response.Status != "ok" {
		return nil, errors.New("tako: network primitive call failed")
	}
	return response.Data, nil
}
