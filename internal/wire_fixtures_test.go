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
		Databases: map[string]DatabaseConnection{
			"main": {URL: "http://127.0.0.1:9845", AuthToken: "token-example", ReplicaAuthToken: "a9bf1b584162d047990e5146e26c50e74285ef9fcc16dcb0c261a7e1996ab6cf"},
		},
		KV: map[string]KVConnection{
			"sessions": {Token: "kv-token-example", ReplicaToken: "82d82a1b045a2668f2470afaa6c8f8b5d0cc7a29cf37176f5a5cf0e89d050b55"},
		},
	}
	if !reflect.DeepEqual(bootstrap, want) {
		t.Fatalf("bootstrap parsed as %#v", bootstrap)
	}
	if _, set := os.LookupEnv(BootstrapDataEnv); set {
		t.Fatalf("%s must be cleared after it is read", BootstrapDataEnv)
	}
}
