package internal

import (
	"os"
	"reflect"
	"testing"
)

// The bootstrap envelope tako-core owns in test-support/wire/ (see its README).
const wireBootstrapFixture = "../test-support/wire/bootstrap.json"

func TestBootstrapFixtureParses(t *testing.T) {
	data, err := os.ReadFile(wireBootstrapFixture)
	if err != nil {
		t.Fatalf("wire fixtures not found (run from a full checkout): %v", err)
	}
	t.Setenv(BootstrapDataEnv, string(data))
	bootstrap := bootstrapFromEnv()
	want := &Bootstrap{
		Token:   "tok-abc",
		Secrets: map[string]string{"DATABASE_URL": "postgres://db/app", "API_KEY": "key-123"},
	}
	if !reflect.DeepEqual(bootstrap, want) {
		t.Fatalf("bootstrap parsed as %#v", bootstrap)
	}
	if _, set := os.LookupEnv(BootstrapDataEnv); set {
		t.Fatalf("%s must be cleared after it is read", BootstrapDataEnv)
	}
}
