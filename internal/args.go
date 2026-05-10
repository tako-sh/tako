package internal

import (
	"os"
)

// Config holds runtime configuration parsed from CLI args and env vars.
type Config struct {
	// AppName is the Tako app identity from TAKO_APP_NAME.
	AppName string
	// InstanceID is the 8-char instance identifier assigned by tako-server.
	InstanceID string
	// Version is the deploy version string from TAKO_BUILD.
	Version string
	// Host is the address to bind to. Defaults to "0.0.0.0".
	Host string
	// Port is the TCP port to listen on. Defaults to "3000".
	Port string
	// InternalToken authenticates Host:<app>.tako requests from tako-server.
	// Delivered on the fd 3 bootstrap envelope. Empty in dev mode (no auth required).
	InternalToken string
}

// ParseConfig reads configuration from os.Args, environment variables,
// and the given bootstrap envelope (may be nil in dev mode).
func ParseConfig(bootstrap *Bootstrap) Config {
	return ParseConfigFrom(os.Args[1:], os.Getenv, bootstrap)
}

// ParseConfigFrom reads configuration from the given args, env lookup, and bootstrap envelope.
func ParseConfigFrom(args []string, getenv func(string) string, bootstrap *Bootstrap) Config {
	cfg := Config{
		Host: "0.0.0.0",
		Port: "3000",
	}

	// Parse CLI args: --instance <id>
	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--instance":
			if i+1 < len(args) {
				i++
				cfg.InstanceID = args[i]
			}
		}
	}

	cfg.Version = getenv("TAKO_BUILD")
	cfg.AppName = getenv("TAKO_APP_NAME")
	if host := getenv("HOST"); host != "" {
		cfg.Host = host
	}
	if port := getenv("PORT"); port != "" {
		cfg.Port = port
	}

	if bootstrap != nil {
		cfg.InternalToken = bootstrap.Token
	}

	return cfg
}
