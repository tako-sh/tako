package internal

import (
	"fmt"
	"os"
)

// ComposeServiceEnv is set by Tako on every Compose service with x-tako.
const ComposeServiceEnv = "TAKO_COMPOSE_SERVICE"

// ComposeSDKError explains why a Tako runtime feature is unavailable in a
// Compose service that isn't marked sdk: true. hasRuntime reports whether
// this process received the Tako runtime. It returns nil outside Compose and
// in sdk: true services, so plain services can still import the SDK.
func ComposeSDKError(hasRuntime bool) error {
	service := os.Getenv(ComposeServiceEnv)
	if service == "" || hasRuntime {
		return nil
	}
	return fmt.Errorf(
		"tako: this Compose service (%q) isn't marked `sdk: true` in x-tako, so it has no Tako runtime for secrets, channels or workflows; add `sdk: true` to services.%s.x-tako in the Compose file",
		service, service,
	)
}
