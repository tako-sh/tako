package internal

import (
	"context"
	"strings"
	"testing"
)

func TestComposeSDKErrorNamesTheMissingMarker(t *testing.T) {
	t.Setenv(ComposeServiceEnv, "web")
	err := ComposeSDKError(false)
	if err == nil || !strings.Contains(err.Error(), "isn't marked `sdk: true` in x-tako") ||
		!strings.Contains(err.Error(), "services.web.x-tako") {
		t.Fatalf("unexpected error: %v", err)
	}
	if err := ComposeSDKError(true); err != nil {
		t.Fatalf("an sdk: true service has the runtime: %v", err)
	}
	t.Setenv(ComposeServiceEnv, "")
	if err := ComposeSDKError(false); err != nil {
		t.Fatalf("processes outside Compose get no hint: %v", err)
	}
}

func TestStoreInvalidationExplainsTheMissingComposeMarker(t *testing.T) {
	t.Setenv(ComposeServiceEnv, "web")
	t.Setenv("TAKO_INTERNAL_SOCKET", "")
	t.Setenv("TAKO_APP_NAME", "")
	err := publishStoreInvalidation(context.Background(), "cart", []byte(`{}`))
	if err == nil || !strings.Contains(err.Error(), "sdk: true") {
		t.Fatalf("unexpected error: %v", err)
	}
}
