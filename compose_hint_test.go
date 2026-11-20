package tako

import (
	"context"
	"strings"
	"testing"
)

func TestComposeServicesWithoutSDKExplainTheMissingMarker(t *testing.T) {
	t.Setenv("TAKO_COMPOSE_SERVICE", "web")
	t.Setenv("TAKO_INTERNAL_SOCKET", "")
	t.Setenv("TAKO_APP_NAME", "")
	if _, err := Publish(context.Background(), "chat", nil, "message", "hi"); err == nil ||
		!strings.Contains(err.Error(), "isn't marked `sdk: true` in x-tako") {
		t.Fatalf("unexpected publish error: %v", err)
	}
	defer func() {
		recovered := recover()
		err, ok := recovered.(error)
		if !ok || !strings.Contains(err.Error(), "services.web.x-tako") {
			t.Fatalf("GetSecret must explain the missing marker, got %v", recovered)
		}
	}()
	GetSecret("API_KEY")
}
