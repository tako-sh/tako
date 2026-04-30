package tako

import (
	"encoding/json"
	"io"
	"net"
	"net/http"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"tako.sh/internal"
)

func TestGetSecretIgnoresEnv(t *testing.T) {
	// GetSecret must not fall back to os.Getenv — the store is the only source.
	secrets.Inject(map[string]string{})
	key := "TAKO_TEST_SECRET_NO_FALLBACK"
	origVal := os.Getenv(key)
	os.Setenv(key, "from-env")
	defer setOrUnset(key, origVal)

	if got := GetSecret(key); got != "" {
		t.Errorf("GetSecret(%q) = %q, want empty (env must not be a fallback)", key, got)
	}
}

func TestGetSecret(t *testing.T) {
	secrets.Inject(map[string]string{"KEY": "value", "OTHER": "data"})

	if got := GetSecret("KEY"); got != "value" {
		t.Errorf("GetSecret(KEY) = %q, want %q", got, "value")
	}
	if got := GetSecret("OTHER"); got != "data" {
		t.Errorf("GetSecret(OTHER) = %q, want %q", got, "data")
	}
	if got := GetSecret("MISSING"); got != "" {
		t.Errorf("GetSecret(MISSING) = %q, want empty", got)
	}
}

func TestMetadata(t *testing.T) {
	configOnce = syncOnce()
	origArgs := os.Args
	origBuild := os.Getenv("TAKO_BUILD")
	os.Args = []string{"test", "--instance", "meta1234"}
	os.Setenv("TAKO_BUILD", "v5.0")
	defer func() {
		os.Args = origArgs
		setOrUnset("TAKO_BUILD", origBuild)
	}()

	if got := InstanceID(); got != "meta1234" {
		t.Errorf("InstanceID() = %q, want %q", got, "meta1234")
	}
	if got := Version(); got != "v5.0" {
		t.Errorf("Version() = %q, want %q", got, "v5.0")
	}
	if got := Uptime(); got <= 0 {
		t.Errorf("Uptime() = %v, want > 0", got)
	}
}

func TestMetadataEmptyInDevMode(t *testing.T) {
	configOnce = syncOnce()
	origArgs := os.Args
	os.Args = []string{"test"}
	defer func() { os.Args = origArgs }()

	if got := InstanceID(); got != "" {
		t.Errorf("InstanceID() = %q, want empty in dev mode", got)
	}
	if got := Version(); got != "" {
		t.Errorf("Version() = %q, want empty in dev mode", got)
	}
}

func TestChannelExportsCompile(t *testing.T) {
	t.Parallel()

	var _ ChannelDefinition = ChannelDefinition{}
	var _ VerifyInput = VerifyInput{}
	var _ ChannelAuthScheme = ChannelAuthScheme{}
	var _ ChannelHeaderValue = ParseChannelHeaderValue("Bearer token")
	var _ = AllowChannel(ChannelGrant{})
	var _ = RejectChannel()

	registry := internal.NewChannelRegistry()
	registry.Register("test", ChannelDefinition{})
}

func TestListenerTCP(t *testing.T) {
	configOnce = syncOnce()
	origArgs := os.Args
	os.Args = []string{"test"}
	origPort := os.Getenv("PORT")
	os.Setenv("PORT", "19876")
	defer func() {
		os.Args = origArgs
		setOrUnset("PORT", origPort)
	}()

	ln, err := Listener()
	if err != nil {
		t.Fatalf("Listener() error: %v", err)
	}
	defer ln.Close()

	if ln.Addr().Network() != "tcp" {
		t.Errorf("network = %q, want tcp", ln.Addr().Network())
	}
}

func TestFullProtocol(t *testing.T) {
	configOnce = syncOnce()
	origArgs := os.Args
	origBuild := os.Getenv("TAKO_BUILD")
	os.Args = []string{"test", "--instance", "full1234"}
	origPort := os.Getenv("PORT")
	origHost := os.Getenv("HOST")
	origBootstrap := bootstrap
	bootstrap = &internal.Bootstrap{Token: "test-token", Secrets: map[string]string{}}
	os.Setenv("TAKO_BUILD", "v3")
	os.Setenv("HOST", "127.0.0.1")
	os.Setenv("PORT", "0")
	defer func() {
		os.Args = origArgs
		setOrUnset("TAKO_BUILD", origBuild)
		setOrUnset("PORT", origPort)
		setOrUnset("HOST", origHost)
		bootstrap = origBootstrap
	}()

	userHandler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html")
		w.Write([]byte("<!doctype html><html><body>hello</body></html>"))
	})

	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()

	cfg := config()
	wrapped := internal.NewEndpointHandler(cfg.InstanceID, cfg.Version, cfg.InternalToken, userHandler)
	go http.Serve(ln, wrapped)
	time.Sleep(10 * time.Millisecond)

	addr := ln.Addr().String()
	client := &http.Client{}

	// Health check (with token)
	req, _ := http.NewRequest("GET", "http://"+addr+"/status", nil)
	req.Host = "tako.internal"
	req.Header.Set("x-tako-internal-token", "test-token")
	resp, err := client.Do(req)
	if err != nil {
		t.Fatalf("health check failed: %v", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		t.Fatalf("health check status = %d, want 200", resp.StatusCode)
	}

	var status internal.StatusResponse
	json.NewDecoder(resp.Body).Decode(&status)
	if status.InstanceID != "full1234" {
		t.Errorf("instance_id = %q, want %q", status.InstanceID, "full1234")
	}

	// User passthrough
	req3, _ := http.NewRequest("GET", "http://"+addr+"/", nil)
	resp3, err := client.Do(req3)
	if err != nil {
		t.Fatal(err)
	}
	defer resp3.Body.Close()

	body, _ := io.ReadAll(resp3.Body)
	if !strings.Contains(string(body), "hello") {
		t.Errorf("body = %q, want to contain %q", string(body), "hello")
	}
}

func TestSignalReadyPortWritesPortToNamedPipe(t *testing.T) {
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer r.Close()
	defer w.Close()

	signalReadyPortToFD(43123, w.Fd())

	out, _ := io.ReadAll(r)
	line := strings.TrimSpace(string(out))
	if line != "43123" {
		t.Errorf("ready signal port = %s, want %s", line, "43123")
	}
}

func setOrUnset(key, value string) {
	if value != "" {
		os.Setenv(key, value)
	} else {
		os.Unsetenv(key)
	}
}

func syncOnce() sync.Once {
	return sync.Once{}
}
