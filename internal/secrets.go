package internal

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"sync"
	"syscall"
)

// Bootstrap is the envelope delivered on fd 3 by tako-server.
//
// The token is the per-instance internal auth token used for
// Host:<app>.tako traffic. Secrets are the user-configured secrets
// for this app. The envelope rides a pipe (not env/args) so neither
// value inherits into subprocesses the app spawns.
type Bootstrap struct {
	Token   string            `json:"token"`
	Secrets map[string]string `json:"secrets"`
}

// BootstrapFromFd3 reads the bootstrap envelope from file descriptor 3.
//
// Returns nil if fd 3 does not exist (EBADF — not running under Tako).
// Exits hard on invalid JSON (broken Tako launch path).
func BootstrapFromFd3() *Bootstrap {
	return bootstrapFromFd(3)
}

// bootstrapFromFd reads the bootstrap envelope from the given file descriptor.
// Extracted for testability (tests can use arbitrary fds without clobbering
// fd 3 which the Go test harness uses).
func bootstrapFromFd(fd int) *Bootstrap {
	f := os.NewFile(uintptr(fd), "tako-bootstrap")
	if f == nil {
		return nil
	}
	defer f.Close()

	data, err := io.ReadAll(f)
	if err != nil {
		if errors.Is(err, syscall.EBADF) {
			return nil
		}
		fmt.Fprintf(os.Stderr, "tako: failed to read bootstrap from fd %d: %v\n", fd, err)
		os.Exit(1)
	}

	var b Bootstrap
	if err := json.Unmarshal(data, &b); err != nil {
		fmt.Fprintf(os.Stderr, "tako: invalid bootstrap JSON on fd %d: %v\n", fd, err)
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
