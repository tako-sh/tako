package tako

import (
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	"tako.sh/internal"
)

func TestGetDatabaseReadsBootstrapBindings(t *testing.T) {
	previous := bootstrap
	t.Cleanup(func() { bootstrap = previous })
	bootstrap = &internal.Bootstrap{
		Token:   "tok",
		Secrets: map[string]string{},
		Databases: map[string]internal.DatabaseConnection{
			"main":  {URL: "http://127.0.0.1:9845", AuthToken: "secret-token"},
			"audit": {URL: "http://127.0.0.1:9845", AuthToken: "other"},
		},
	}

	main, ok := GetDB("main")
	if !ok || main.URL != "http://127.0.0.1:9845" || main.AuthToken != "secret-token" {
		t.Fatalf("GetDB(main) = %v, %v", main, ok)
	}
	if got := main.DSN(); got != "http://127.0.0.1:9845?authToken=secret-token" {
		t.Fatalf("DSN() = %q", got)
	}
	encoded, err := json.Marshal(map[string]DB{"main": main})
	if err != nil {
		t.Fatalf("json.Marshal: %v", err)
	}
	for _, text := range []string{main.String(), fmt.Sprintf("%v", main), fmt.Sprintf("%#v", main), string(encoded)} {
		if strings.Contains(text, "secret-token") {
			t.Fatalf("formatted database leaks its token: %s", text)
		}
	}
	if !strings.Contains(string(encoded), "http://127.0.0.1:9845") {
		t.Fatalf("encoded database lost its URL: %s", encoded)
	}
	if _, ok := GetDB("missing"); ok {
		t.Fatal("GetDB(missing) must report false")
	}
	if got := DBNames(); strings.Join(got, ",") != "audit,main" {
		t.Fatalf("DBNames() = %v", got)
	}
}

func TestGetDatabaseWithoutBootstrap(t *testing.T) {
	previous := bootstrap
	t.Cleanup(func() { bootstrap = previous })
	bootstrap = nil
	if _, ok := GetDB("main"); ok {
		t.Fatal("no bootstrap means no databases")
	}
	if DBNames() != nil {
		t.Fatal("no bootstrap means no names")
	}
}
