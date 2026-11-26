package tako

import (
	"encoding/json"
	"fmt"
	"net/url"
	"sort"
)

// DB is a connection to a Tako-hosted database bound to this app in
// tako.toml (`[envs.<env>].db`). Tako serves the database over the
// libSQL HTTP protocol next to the app, in development and in production.
//
// Use it with the libSQL driver:
//
//	import _ "github.com/tursodatabase/libsql-client-go/libsql"
//
//	main, ok := tako.GetDB("main")
//	if !ok {
//		log.Fatal("no database bound as main")
//	}
//	db, err := sql.Open("libsql", main.DSN())
type DB struct {
	// URL is the database endpoint.
	URL string
	// AuthToken authorizes this app's binding. Treat it as a secret.
	AuthToken        string
	replicaAuthToken string
}

// ReadReplica returns a read-only connection served on this app server.
// A runtime predating replica bindings returns false.
func (d DB) ReadReplica() (DB, bool) {
	if d.replicaAuthToken == "" {
		return DB{}, false
	}
	return DB{URL: d.URL, AuthToken: d.replicaAuthToken}, true
}

// DSN returns the URL with the token as the `authToken` query parameter,
// the form libSQL drivers accept.
func (d DB) DSN() string {
	u, err := url.Parse(d.URL)
	if err != nil {
		return d.URL
	}
	query := u.Query()
	query.Set("authToken", d.AuthToken)
	u.RawQuery = query.Encode()
	return u.String()
}

// String redacts the token so logging a DB does not leak it.
func (d DB) String() string {
	return fmt.Sprintf("tako.DB{URL: %q, AuthToken: [REDACTED]}", d.URL)
}

// GoString redacts the token for %#v as well.
func (d DB) GoString() string {
	return d.String()
}

// MarshalJSON redacts the token, so encoding a DB as JSON for logs or
// diagnostics does not leak it. Pass DSN() to drivers instead.
func (d DB) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		URL       string `json:"url"`
		AuthToken string `json:"authToken"`
	}{URL: d.URL, AuthToken: "[REDACTED]"})
}

// GetDB returns the database bound to this app as name.
func GetDB(name string) (DB, bool) {
	if bootstrap == nil {
		return DB{}, false
	}
	connection, ok := bootstrap.Databases[name]
	if !ok {
		return DB{}, false
	}
	return DB{URL: connection.URL, AuthToken: connection.AuthToken, replicaAuthToken: connection.ReplicaAuthToken}, true
}

// DBNames returns the names of this app's database bindings, sorted.
func DBNames() []string {
	if bootstrap == nil {
		return nil
	}
	names := make([]string, 0, len(bootstrap.Databases))
	for name := range bootstrap.Databases {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}
