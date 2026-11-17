package tako

import (
	"encoding/json"
	"fmt"
	"net/url"
	"sort"
)

// Database is a connection to a Tako-hosted database bound to this app in
// tako.toml (`[envs.<env>].databases`). Tako serves the database over the
// libSQL HTTP protocol next to the app, in development and in production.
//
// Use it with the libSQL driver:
//
//	import _ "github.com/tursodatabase/libsql-client-go/libsql"
//
//	main, ok := tako.GetDatabase("main")
//	if !ok {
//		log.Fatal("no database bound as main")
//	}
//	db, err := sql.Open("libsql", main.DSN())
type Database struct {
	// URL is the database endpoint.
	URL string
	// AuthToken authorizes this app's binding. Treat it as a secret.
	AuthToken string
}

// DSN returns the URL with the token as the `authToken` query parameter,
// the form libSQL drivers accept.
func (d Database) DSN() string {
	u, err := url.Parse(d.URL)
	if err != nil {
		return d.URL
	}
	query := u.Query()
	query.Set("authToken", d.AuthToken)
	u.RawQuery = query.Encode()
	return u.String()
}

// String redacts the token so logging a Database does not leak it.
func (d Database) String() string {
	return fmt.Sprintf("tako.Database{URL: %q, AuthToken: [REDACTED]}", d.URL)
}

// GoString redacts the token for %#v as well.
func (d Database) GoString() string {
	return d.String()
}

// MarshalJSON redacts the token, so encoding a Database as JSON for logs or
// diagnostics does not leak it. Pass DSN() to drivers instead.
func (d Database) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		URL       string `json:"url"`
		AuthToken string `json:"authToken"`
	}{URL: d.URL, AuthToken: "[REDACTED]"})
}

// GetDatabase returns the database bound to this app as name.
func GetDatabase(name string) (Database, bool) {
	if bootstrap == nil {
		return Database{}, false
	}
	connection, ok := bootstrap.Databases[name]
	if !ok {
		return Database{}, false
	}
	return Database{URL: connection.URL, AuthToken: connection.AuthToken}, true
}

// DatabaseNames returns the names of this app's database bindings, sorted.
func DatabaseNames() []string {
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
