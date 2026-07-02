package internal

import (
	"os"
	"sync"
	"testing"
)

func TestSecretStoreGetEmpty(t *testing.T) {
	s := NewSecretStore()
	if got := s.Get("missing"); got != "" {
		t.Errorf("Get(missing) = %q, want empty", got)
	}
}

func TestSecretStoreInjectAndGet(t *testing.T) {
	s := NewSecretStore()
	s.Inject(map[string]string{"DB_URL": "postgres://...", "API_KEY": "secret123"})

	if got := s.Get("DB_URL"); got != "postgres://..." {
		t.Errorf("Get(DB_URL) = %q, want %q", got, "postgres://...")
	}
	if got := s.Get("API_KEY"); got != "secret123" {
		t.Errorf("Get(API_KEY) = %q, want %q", got, "secret123")
	}
}

func TestSecretStoreAll(t *testing.T) {
	s := NewSecretStore()
	s.Inject(map[string]string{"A": "1", "B": "2"})

	all := s.All()
	if len(all) != 2 {
		t.Fatalf("All() returned %d entries, want 2", len(all))
	}

	// Verify it's a copy — modifying the returned map shouldn't affect the store
	all["A"] = "modified"
	if got := s.Get("A"); got != "1" {
		t.Errorf("store was modified through All() return value")
	}
}

func TestSecretStoreString(t *testing.T) {
	s := NewSecretStore()
	s.Inject(map[string]string{"KEY": "value"})

	if got := s.String(); got != "[REDACTED]" {
		t.Errorf("String() = %q, want %q", got, "[REDACTED]")
	}
}

func TestSecretStoreConcurrent(t *testing.T) {
	s := NewSecretStore()
	s.Inject(map[string]string{"KEY": "initial"})

	var wg sync.WaitGroup
	for i := 0; i < 100; i++ {
		wg.Add(2)
		go func() {
			defer wg.Done()
			_ = s.Get("KEY")
		}()
		go func() {
			defer wg.Done()
			s.Inject(map[string]string{"KEY": "updated"})
		}()
	}
	wg.Wait()
}

func TestBootstrapFromFdWithPipe(t *testing.T) {
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}

	_, err = w.WriteString(`{"token":"tok-xyz","secrets":{"DB_URL":"postgres://test","API_KEY":"key123"}}`)
	if err != nil {
		r.Close()
		w.Close()
		t.Fatal(err)
	}
	w.Close()

	b := bootstrapFromFd(int(r.Fd()))
	if b == nil {
		t.Fatal("bootstrapFromFd() returned nil, want envelope")
	}
	if b.Token != "tok-xyz" {
		t.Errorf("Token = %q, want %q", b.Token, "tok-xyz")
	}
	if got := b.Secrets["DB_URL"]; got != "postgres://test" {
		t.Errorf("DB_URL = %q, want %q", got, "postgres://test")
	}
	if got := b.Secrets["API_KEY"]; got != "key123" {
		t.Errorf("API_KEY = %q, want %q", got, "key123")
	}
}

func TestBootstrapFromFdReturnsNilOnBadFd(t *testing.T) {
	b := bootstrapFromFd(9999)
	if b != nil {
		t.Errorf("bootstrapFromFd(9999) = %v, want nil (EBADF)", b)
	}
}

func TestBootstrapFromFdIgnoresNonFifoFd(t *testing.T) {
	// A foreign inherited fd (e.g. a regular file under a CI harness) must
	// be left alone: not read, not closed, and never treated as bootstrap
	// data even if it contains non-JSON content.
	f, err := os.CreateTemp(t.TempDir(), "not-a-pipe")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	if _, err := f.WriteString("definitely not json"); err != nil {
		t.Fatal(err)
	}

	b := bootstrapFromFd(int(f.Fd()))
	if b != nil {
		t.Errorf("bootstrapFromFd(regular file) = %v, want nil", b)
	}

	// The fd must still be usable — the guard must not have closed it.
	if _, err := f.Seek(0, 0); err != nil {
		t.Errorf("fd was closed by bootstrapFromFd: %v", err)
	}
}

func TestBootstrapFromFdEmptySecrets(t *testing.T) {
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	_, err = w.WriteString(`{"token":"only","secrets":{}}`)
	if err != nil {
		r.Close()
		w.Close()
		t.Fatal(err)
	}
	w.Close()

	b := bootstrapFromFd(int(r.Fd()))
	if b == nil || b.Token != "only" || len(b.Secrets) != 0 {
		t.Fatalf("envelope with empty secrets: got %#v", b)
	}
}

func TestBootstrapFromEnvRemovesEnv(t *testing.T) {
	t.Setenv(BootstrapDataEnv, `{"token":"env-token","secrets":{"KEY":"value"}}`)

	b := bootstrapFromEnv()
	if b == nil {
		t.Fatal("bootstrapFromEnv() returned nil, want envelope")
	}
	if b.Token != "env-token" {
		t.Errorf("Token = %q, want %q", b.Token, "env-token")
	}
	if got := b.Secrets["KEY"]; got != "value" {
		t.Errorf("KEY = %q, want %q", got, "value")
	}
	if got := os.Getenv(BootstrapDataEnv); got != "" {
		t.Errorf("%s still set after read: %q", BootstrapDataEnv, got)
	}
}

func TestBootstrapFromRuntimePrefersFdOverEnv(t *testing.T) {
	t.Setenv(BootstrapDataEnv, `{"token":"env-token","secrets":{"KEY":"env"}}`)

	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	_, err = w.WriteString(`{"token":"fd-token","secrets":{"KEY":"fd"}}`)
	if err != nil {
		r.Close()
		w.Close()
		t.Fatal(err)
	}
	w.Close()

	b := bootstrapFromRuntimeFd(int(r.Fd()))
	if b == nil {
		t.Fatal("bootstrapFromRuntimeFd() returned nil, want envelope")
	}
	if b.Token != "fd-token" {
		t.Errorf("Token = %q, want %q", b.Token, "fd-token")
	}
	if got := b.Secrets["KEY"]; got != "fd" {
		t.Errorf("KEY = %q, want %q", got, "fd")
	}
	if got := os.Getenv(BootstrapDataEnv); got != "" {
		t.Errorf("%s still set after fd bootstrap won: %q", BootstrapDataEnv, got)
	}
}
