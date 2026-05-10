package internal

import (
	"testing"
)

func TestParseConfigFromArgsAndEnv(t *testing.T) {
	args := []string{"--instance", "abcd1234"}
	cfg := ParseConfigFrom(args, func(key string) string {
		switch key {
		case "TAKO_BUILD":
			return "v1.0"
		case "TAKO_APP_NAME":
			return "demo"
		default:
			return ""
		}
	}, &Bootstrap{Token: "tok-xyz"})

	if cfg.AppName != "demo" {
		t.Errorf("AppName = %q, want %q", cfg.AppName, "demo")
	}
	if cfg.InstanceID != "abcd1234" {
		t.Errorf("InstanceID = %q, want %q", cfg.InstanceID, "abcd1234")
	}
	if cfg.Version != "v1.0" {
		t.Errorf("Version = %q, want %q", cfg.Version, "v1.0")
	}
	if cfg.InternalToken != "tok-xyz" {
		t.Errorf("InternalToken = %q, want %q", cfg.InternalToken, "tok-xyz")
	}
}

func TestParseConfigPortAndHostFromEnv(t *testing.T) {
	cfg := ParseConfigFrom(nil, func(key string) string {
		switch key {
		case "PORT":
			return "8080"
		case "HOST":
			return "127.0.0.1"
		default:
			return ""
		}
	}, nil)

	if cfg.Port != "8080" {
		t.Errorf("Port = %q, want %q", cfg.Port, "8080")
	}
	if cfg.Host != "127.0.0.1" {
		t.Errorf("Host = %q, want %q", cfg.Host, "127.0.0.1")
	}
}

func TestParseConfigDefaults(t *testing.T) {
	cfg := ParseConfigFrom(nil, func(string) string { return "" }, nil)

	if cfg.Port != "3000" {
		t.Errorf("Port = %q, want %q", cfg.Port, "3000")
	}
	if cfg.Host != "0.0.0.0" {
		t.Errorf("Host = %q, want %q", cfg.Host, "0.0.0.0")
	}
}

func TestParseConfigDevMode(t *testing.T) {
	cfg := ParseConfigFrom(nil, func(string) string { return "" }, nil)

	if cfg.InstanceID != "" {
		t.Errorf("InstanceID should be empty in dev mode, got %q", cfg.InstanceID)
	}
}
