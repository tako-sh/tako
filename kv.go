package tako

import (
	"encoding/json"
	"fmt"
	"sort"
	"strings"
)

// KV is a connection to a Tako-hosted KV store bound to this app in
// tako.toml (`[envs.<env>].kv`). Tako serves the store over the Redis
// protocol next to the app, in development and in production.
//
// Use it with any Redis client, such as go-redis:
//
//	import "github.com/redis/go-redis/v9"
//
//	sessions, ok := tako.GetKV("sessions")
//	if !ok {
//		log.Fatal("no KV store bound as sessions")
//	}
//	opt, err := redis.ParseURL(sessions.URL)
//	if err != nil {
//		log.Fatal(err)
//	}
//	client := redis.NewClient(opt)
type KV struct {
	// URL is the redis:// endpoint of the store. Its password is the token
	// that authorizes this app's binding, so treat it as a secret.
	URL string
}

// String redacts the token so logging a KV does not leak it.
func (k KV) String() string {
	return fmt.Sprintf("tako.KV{URL: %q}", redactKVURL(k.URL))
}

// GoString redacts the token for %#v as well.
func (k KV) GoString() string {
	return k.String()
}

// MarshalJSON redacts the token, so encoding a KV as JSON for logs or
// diagnostics does not leak it. Pass URL to clients instead.
func (k KV) MarshalJSON() ([]byte, error) {
	return json.Marshal(struct {
		URL string `json:"url"`
	}{URL: redactKVURL(k.URL)})
}

// redactKVURL turns redis://:<token>@host:port into
// redis://:[REDACTED]@host:port. It cuts at the last @ so a password that
// contains one never leaks a part of it.
func redactKVURL(raw string) string {
	at := strings.LastIndex(raw, "@")
	if at < 0 {
		return "[REDACTED]"
	}
	scheme := ""
	if end := strings.Index(raw, "://"); end >= 0 && end < at {
		scheme = raw[:end+3]
	}
	return scheme + ":[REDACTED]@" + raw[at+1:]
}

// GetKV returns the KV store bound to this app as name.
func GetKV(name string) (KV, bool) {
	if bootstrap == nil {
		return KV{}, false
	}
	connection, ok := bootstrap.KV[name]
	if !ok {
		return KV{}, false
	}
	return KV{URL: connection.URL}, true
}

// KVNames returns the names of this app's KV store bindings, sorted.
func KVNames() []string {
	if bootstrap == nil {
		return nil
	}
	names := make([]string, 0, len(bootstrap.KV))
	for name := range bootstrap.KV {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}
