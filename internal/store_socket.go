package internal

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"time"
)

func publishStoreInvalidation(ctx context.Context, name string, params json.RawMessage, lifecycle ChannelLifecycleConfig) error {
	socketPath, app := os.Getenv("TAKO_INTERNAL_SOCKET"), os.Getenv("TAKO_APP_NAME")
	if socketPath == "" || app == "" {
		return errors.New("tako: store invalidation requires TAKO_INTERNAL_SOCKET and TAKO_APP_NAME")
	}
	command := struct {
		Command string `json:"command"`
		App     string `json:"app"`
		Channel string `json:"channel"`
		Payload struct {
			Type        string                 `json:"type"`
			Data        struct{}               `json:"data"`
			ScopeParams json.RawMessage        `json:"scopeParams"`
			Lifecycle   ChannelLifecycleConfig `json:"lifecycle"`
		} `json:"payload"`
	}{Command: "channel_publish", App: app, Channel: storeChannelPrefix + name}
	command.Payload.Type = "tako.store.invalidate"
	command.Payload.ScopeParams = params
	command.Payload.Lifecycle = lifecycle
	body, err := json.Marshal(command)
	if err != nil {
		return fmt.Errorf("tako: encode store invalidation: %w", err)
	}
	if len(body) > maxInternalBodyBytes {
		return errors.New("tako: store invalidation is too large")
	}
	// Bound calls even without a caller deadline and honor cancellation during
	// reads and writes, not just while connecting. Publication is attempted once.
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	conn, err := (&net.Dialer{Timeout: 5 * time.Second}).DialContext(ctx, "unix", socketPath)
	if err != nil {
		return fmt.Errorf("tako: connect store invalidation socket: %w", err)
	}
	defer conn.Close()
	deadline, _ := ctx.Deadline()
	if err := conn.SetDeadline(deadline); err != nil {
		return fmt.Errorf("tako: set store invalidation deadline: %w", err)
	}
	stop := context.AfterFunc(ctx, func() { _ = conn.Close() })
	defer stop()
	if _, err := io.Copy(conn, bytes.NewReader(append(body, '\n'))); err != nil {
		return storeSocketError(ctx, "write store invalidation", err)
	}
	reader := bufio.NewReader(io.LimitReader(conn, maxInternalBodyBytes+1))
	line, err := reader.ReadBytes('\n')
	if err != nil || len(line) > maxInternalBodyBytes {
		if err == nil {
			err = errors.New("response exceeds limit")
		}
		return storeSocketError(ctx, "read store invalidation response", err)
	}
	var response struct {
		Status string `json:"status"`
	}
	if err := json.Unmarshal(line, &response); err != nil {
		return errors.New("tako: invalid store invalidation response")
	}
	if response.Status != "ok" {
		// The socket's private diagnostic may contain backend details. Keep the
		// public error stable and safe for application error responses.
		return errors.New("tako: store invalidation failed")
	}
	return nil
}

func storeSocketError(ctx context.Context, operation string, err error) error {
	if ctx.Err() != nil {
		return fmt.Errorf("tako: %s: %w", operation, ctx.Err())
	}
	return fmt.Errorf("tako: %s: %w", operation, err)
}
