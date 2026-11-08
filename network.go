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
// It acknowledges publication; it does not wait for subscribers to process it.
func Publish(ctx context.Context, address string, params any, messageType string, data any) (json.RawMessage, error) {
	if params == nil {
		params = map[string]any{}
	}
	return callNetwork(ctx, address, map[string]any{"operation": "publish", "params": params, "message": map[string]any{"type": messageType, "data": data}})
}

func callNetwork(ctx context.Context, address string, request any) (json.RawMessage, error) {
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
