package internal

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sync"
	"syscall"
)

const BootstrapDataEnv = "TAKO_BOOTSTRAP_DATA"

// Bootstrap is the envelope delivered on fd 3 by tako-server.
//
// The token is the per-instance internal auth token used for
// Host:<app>.tako traffic. Secrets are the user-configured secrets
// for this app. Native processes receive the envelope on fd 3 so it
// is not inherited through env/args. Container processes receive the
// same envelope through TAKO_BOOTSTRAP_DATA because fd 3 does not
// cross the container boundary in v0.
type Bootstrap struct {
	Token   string            `json:"token"`
	Secrets map[string]string `json:"secrets"`
}

// BootstrapFromRuntime reads the bootstrap envelope from the current process.
//
// Native fd 3 takes precedence. TAKO_BOOTSTRAP_DATA is only the fallback
// transport for containers and is removed from the process environment after
// it is read.
func BootstrapFromRuntime() *Bootstrap {
	return bootstrapFromRuntimeFd(3)
}

// BootstrapFromFd3 reads the bootstrap envelope from file descriptor 3.
//
// Returns nil if fd 3 does not exist (EBADF — not running under Tako).
// Exits hard on invalid JSON (broken Tako launch path).
func BootstrapFromFd3() *Bootstrap {
	return bootstrapFromFd(3)
}

func bootstrapFromRuntimeFd(fd int) *Bootstrap {
	if b := bootstrapFromFd(fd); b != nil {
		clearBootstrapEnv()
		return b
	}
	return bootstrapFromEnv()
}

func bootstrapFromEnv() *Bootstrap {
	data := os.Getenv(BootstrapDataEnv)
	if data == "" {
		return nil
	}
	clearBootstrapEnv()
	return parseBootstrap([]byte(data), BootstrapDataEnv)
}

func clearBootstrapEnv() {
	if err := os.Unsetenv(BootstrapDataEnv); err != nil {
		fmt.Fprintf(os.Stderr, "tako: failed to clear %s: %v\n", BootstrapDataEnv, err)
		os.Exit(1)
	}
}

// bootstrapFromFd reads the bootstrap envelope from the given file descriptor.
// Extracted for testability (tests can use arbitrary fds without clobbering
// fd 3 which the Go test harness uses).
func bootstrapFromFd(fd int) *Bootstrap {
	// Stat through the syscall layer before wrapping the fd, mirroring the
	// fd-4 ready signal: tako-server always delivers the envelope on a pipe,
	// and a foreign inherited fd (e.g. under a CI harness) must not be read,
	// closed, or handed to Go's file-finalizer machinery.
	var st syscall.Stat_t
	if err := syscall.Fstat(fd, &st); err != nil {
		return nil
	}
	if st.Mode&syscall.S_IFMT != syscall.S_IFIFO {
		return nil
	}

	f := os.NewFile(uintptr(fd), "tako-bootstrap")
	if f == nil {
		return nil
	}
	defer f.Close()

	data, err := io.ReadAll(f)
	if err != nil {
		fmt.Fprintf(os.Stderr, "tako: failed to read bootstrap from fd %d: %v\n", fd, err)
		os.Exit(1)
	}

	return parseBootstrap(data, fmt.Sprintf("fd %d", fd))
}

func parseBootstrap(data []byte, source string) *Bootstrap {
	var b Bootstrap
	if err := json.Unmarshal(data, &b); err != nil {
		fmt.Fprintf(os.Stderr, "tako: invalid bootstrap JSON from %s: %v\n", source, err)
		os.Exit(1)
	}
	if b.Secrets == nil {
		b.Secrets = map[string]string{}
	}
	return &b
}

// SecretStore is a thread-safe store for Tako-managed secrets.
type SecretStore struct {
	mu      sync.RWMutex
	secrets map[string]string
}

// NewSecretStore creates an empty secret store.
func NewSecretStore() *SecretStore {
	return &SecretStore{
		secrets: make(map[string]string),
	}
}

// Get returns a single secret value by name.
// Returns empty string if the secret doesn't exist.
func (s *SecretStore) Get(name string) string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.secrets[name]
}

// All returns a copy of all secrets.
func (s *SecretStore) All() map[string]string {
	s.mu.RLock()
	defer s.mu.RUnlock()
	out := make(map[string]string, len(s.secrets))
	for k, v := range s.secrets {
		out[k] = v
	}
	return out
}

// Inject replaces all secrets with the given map.
func (s *SecretStore) Inject(secrets map[string]string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.secrets = secrets
}

// String returns "[REDACTED]" to prevent accidental logging.
func (s *SecretStore) String() string {
	return "[REDACTED]"
}
