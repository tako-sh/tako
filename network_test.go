package tako

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"path/filepath"
	"testing"
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

func TestNetworkCallsRequireManagedRuntime(t *testing.T) {
	t.Setenv("TAKO_INTERNAL_SOCKET", "")
	if _, err := Enqueue(context.Background(), "mail.send", nil, NetworkEnqueueOptions{}); err == nil {
		t.Fatal("expected missing runtime error")
	}
}
