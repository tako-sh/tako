package tako

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"path/filepath"
	"testing"
	"time"
)

func TestNetworkCallsKeepCallerIdentity(t *testing.T) {
	path := filepath.Join(t.TempDir(), "internal.sock")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	t.Setenv("TAKO_INTERNAL_SOCKET", path)
	t.Setenv("TAKO_APP_NAME", "go-app/staging")
	received := make(chan map[string]any, 2)
	go func() {
		for i := 0; i < 2; i++ {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			line, _ := bufio.NewReader(conn).ReadBytes('\n')
			var request map[string]any
			_ = json.Unmarshal(line, &request)
			received <- request
			_, _ = conn.Write([]byte("{\"status\":\"ok\",\"data\":{\"id\":\"run-1\",\"deduplicated\":false}}\n"))
			_ = conn.Close()
		}
	}()
	result, err := Enqueue(context.Background(), "mail.send-email", map[string]any{"to": "a@example.com"}, NetworkEnqueueOptions{})
	if err != nil || result.ID != "run-1" {
		t.Fatalf("enqueue: %#v, %v", result, err)
	}
	if _, err := Publish(context.Background(), "web.progress", nil, "update", map[string]any{"percent": 50}); err != nil {
		t.Fatal(err)
	}
	for _, address := range []string{"mail.send-email", "web.progress"} {
		command := <-received
		if command["app"] != "go-app/staging" || command["address"] != address || command["command"] != "network_call" {
			t.Fatalf("wrong envelope: %#v", command)
		}
	}
}

func TestDirectCallAndWorkflowResults(t *testing.T) {
	path := filepath.Join(t.TempDir(), "results.sock")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	t.Setenv("TAKO_INTERNAL_SOCKET", path)
	t.Setenv("TAKO_APP_NAME", "web/staging")
	replies := []string{`{"user":"Ada"}`, `{"status":"succeeded","result":{"url":"report.pdf"},"error":null}`, `{"status":"dead","result":null,"error":"failed"}`, `null`, `{"status":"pending","result":null,"error":null}`}
	operations := make(chan string, len(replies))
	go func() {
		for _, reply := range replies {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			line, _ := bufio.NewReader(conn).ReadBytes('\n')
			var request struct {
				Request struct {
					Operation string `json:"operation"`
				} `json:"request"`
			}
			_ = json.Unmarshal(line, &request)
			operations <- request.Request.Operation
			_, _ = conn.Write([]byte(`{"status":"ok","data":` + reply + "}\n"))
			_ = conn.Close()
		}
	}()
	data, err := Call(context.Background(), "users.lookup", map[string]string{"id": "u1"})
	if err != nil || string(data) != `{"user":"Ada"}` {
		t.Fatalf("call: %s %v", data, err)
	}
	data, err = Result(context.Background(), "reports.generate", "id")
	if err != nil || string(data) != `{"url":"report.pdf"}` {
		t.Fatalf("result: %s %v", data, err)
	}
	for _, status := range []string{"dead", "missing"} {
		_, err := Result(context.Background(), "reports.generate", "id")
		outcome, ok := err.(*WorkflowResultError)
		if !ok || outcome.Status != status {
			t.Fatalf("expected %s: %v", status, err)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	if _, err := Result(ctx, "reports.generate", "id"); err != context.DeadlineExceeded {
		t.Fatalf("timeout: %v", err)
	}
	for i := range replies {
		expected := "result"
		if i == 0 {
			expected = "call"
		}
		if got := <-operations; got != expected {
			t.Fatalf("operation: %s", got)
		}
	}
}

func TestNetworkCallsRequireManagedRuntime(t *testing.T) {
	t.Setenv("TAKO_INTERNAL_SOCKET", "")
	if _, err := Enqueue(context.Background(), "mail.send", nil, NetworkEnqueueOptions{}); err == nil {
		t.Fatal("expected missing runtime error")
	}
}
