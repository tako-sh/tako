// Package tako is the Tako SDK for Go.
//
// It handles the Tako protocol (TCP serving, health checks, secrets)
// so your Go app can be deployed and managed by Tako.
//
// # Quick Start
//
// Most Go frameworks implement [http.Handler] and work directly with
// [ListenAndServe]:
//
//	mux := http.NewServeMux()
//	mux.HandleFunc("/", handler)
//	tako.ListenAndServe(mux)
//
// This also works with Gin, Echo, Chi, gorilla/mux, and any other framework
// that implements [http.Handler].
//
// # Secrets
//
// Secrets are accessed via a generated Secrets struct in tako_secrets.go.
// Run `tako generate` to generate it from your project's secret definitions:
//
//	db := Secrets.DatabaseUrl()
//	key := Secrets.ApiKey()
//
// # Fiber
//
// Fiber uses fasthttp (not net/http), so use [Listener] directly:
//
//	ln, _ := tako.Listener()
//	app := fiber.New()
//	app.Listener(ln)
package tako

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/signal"
	"sync"
	"syscall"
	"time"

	"tako.sh/internal"
)

var (
	// bootstrap is the fd 3 envelope, read once at init. Nil in dev mode
	// (no Tako-managed fd 3).
	bootstrap = internal.BootstrapFromFd3()
	// secrets is populated from the bootstrap envelope.
	secrets = func() *internal.SecretStore {
		s := internal.NewSecretStore()
		if bootstrap != nil {
			s.Inject(bootstrap.Secrets)
		}
		return s
	}()
	configOnce sync.Once
	configVal  internal.Config
	startTime  = time.Now()
)

func config() internal.Config {
	configOnce.Do(func() {
		configVal = internal.ParseConfig(bootstrap)
	})
	return configVal
}

// ListenAndServe wraps the given handler with Tako protocol support and starts
// serving. It blocks until the server shuts down.
//
// Handles SIGTERM and SIGINT for graceful shutdown — in-flight requests are
// given 10 seconds to complete before the server force-closes. This is important
// for rolling deploys where tako-server sends SIGTERM to old instances.
//
// The app listens on HOST:PORT (from environment variables, defaulting to
// 0.0.0.0:3000). In production, tako-server sets these to the assigned
// address for the instance.
//
// Works with any [http.Handler]:
//
//	// net/http
//	mux := http.NewServeMux()
//	tako.ListenAndServe(mux)
//
//	// Gin
//	r := gin.Default()
//	tako.ListenAndServe(r)
//
//	// Echo
//	e := echo.New()
//	tako.ListenAndServe(e)
//
//	// Chi
//	r := chi.NewRouter()
//	tako.ListenAndServe(r)
func ListenAndServe(handler http.Handler) error {
	ln, err := Listener()
	if err != nil {
		return err
	}

	cfg := config()
	wrapped := internal.NewEndpointHandler(
		cfg.AppName,
		cfg.InstanceID,
		cfg.Version,
		cfg.InternalToken,
		handler,
	)

	srv := &http.Server{Handler: wrapped}

	// Graceful shutdown on SIGTERM/SIGINT
	shutdownCh := make(chan os.Signal, 1)
	signal.Notify(shutdownCh, syscall.SIGTERM, syscall.SIGINT)
	defer signal.Stop(shutdownCh)

	errCh := make(chan error, 1)
	go func() {
		errCh <- srv.Serve(ln)
	}()

	select {
	case err := <-errCh:
		if err == http.ErrServerClosed {
			return nil
		}
		return err
	case <-shutdownCh:
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		return srv.Shutdown(ctx)
	}
}

// Listener returns a [net.Listener] configured for the Tako environment.
//
// Listens on HOST:PORT from environment variables (defaults to 0.0.0.0:3000).
// In production, tako-server sets these to the instance's assigned address.
//
// Use this for frameworks that manage their own server lifecycle, like Fiber:
//
//	ln, err := tako.Listener()
//	if err != nil {
//	    log.Fatal(err)
//	}
//	app := fiber.New()
//	app.Listener(ln)
func Listener() (net.Listener, error) {
	cfg := config()
	addr := net.JoinHostPort(cfg.Host, cfg.Port)
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("tako: failed to listen on %s: %w", addr, err)
	}

	if tcpAddr, ok := ln.Addr().(*net.TCPAddr); ok {
		signalReadyPort(tcpAddr.Port)
	}

	return ln, nil
}

func signalReadyPort(port int) {
	// Only touch fd 4 when we know we're under tako-server. The server
	// sets PORT=0 and HOST=127.0.0.1 when spawning; outside that
	// contract, fd 4 may belong to the Go runtime (e.g. kqueue on
	// macOS) and writing/closing it crashes the process.
	if os.Getenv("PORT") != "0" {
		return
	}
	signalReadyPortToFD(port, 4)
}

func signalReadyPortToFD(port int, fd uintptr) {
	// Stat through the syscall layer so we do NOT give the fd to Go's
	// file-finalizer machinery unless we know it's a FIFO. Wrapping a
	// non-Tako fd (like Go's kqueue/epoll fd) in os.NewFile would let
	// a stray GC close it and break the runtime.
	var st syscall.Stat_t
	if err := syscall.Fstat(int(fd), &st); err != nil {
		return
	}
	if st.Mode&syscall.S_IFMT != syscall.S_IFIFO {
		return
	}

	ready := os.NewFile(fd, "tako-ready")
	if ready == nil {
		return
	}
	defer ready.Close()

	_, _ = fmt.Fprintf(ready, "%d\n", port)
}

// InstanceID returns the Tako instance identifier assigned by tako-server.
// Returns an empty string in development mode.
//
// Useful for structured logging and distributed tracing:
//
//	slog.Info("request handled",
//	    "instance", tako.InstanceID(),
//	    "path", r.URL.Path,
//	)
func InstanceID() string {
	return config().InstanceID
}

// Version returns the deploy version string.
// Returns an empty string in development mode.
//
// Useful for logging, health endpoints, and error reporting:
//
//	slog.Info("server started", "version", tako.Version())
func Version() string {
	return config().Version
}

// Uptime returns how long since the process started.
//
//	slog.Info("status", "uptime", tako.Uptime())
func Uptime() time.Duration {
	return time.Since(startTime)
}

// GetSecret returns a secret value by name. This is called by generated code
// in tako_secrets.go — use the typed Secrets struct instead of calling this
// directly.
//
// Secrets are loaded from fd 3 at process startup in both dev and production.
// If a secret is not defined, GetSecret returns an empty string.
//
// Run `tako generate` to generate the Secrets struct:
//
//	// Generated in tako_secrets.go — use this:
//	db := Secrets.DatabaseUrl()
//
//	// Instead of this:
//	db := tako.GetSecret("DATABASE_URL")
func GetSecret(name string) string {
	return secrets.Get(name)
}

func int64ToString(value int64) string {
	return fmt.Sprintf("%d", value)
}
