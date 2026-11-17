// Command go-database runs under `tako run` and drives the bound database
// through the Tako Go SDK and the libSQL database/sql driver.
package main

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"log"
	"strings"

	_ "github.com/tursodatabase/libsql-client-go/libsql"
	"tako.sh"
)

func main() {
	main, ok := tako.GetDatabase("main")
	if !ok {
		log.Fatal("no database is bound as main")
	}
	db, err := sql.Open("libsql", main.DSN())
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	must := func(err error) {
		if err != nil {
			log.Fatal(err)
		}
	}

	_, err = db.Exec("CREATE TABLE IF NOT EXISTS go_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, data BLOB)")
	must(err)
	result, err := db.Exec("INSERT INTO go_items (name, data) VALUES (?, ?)", "gopher", []byte{1, 2, 3})
	must(err)
	id, err := result.LastInsertId()
	must(err)

	var name string
	var data []byte
	must(db.QueryRow("SELECT name, data FROM go_items WHERE id = ?", id).Scan(&name, &data))

	tx, err := db.Begin()
	must(err)
	_, err = tx.Exec("INSERT INTO go_items (name) VALUES ('rolled back')")
	must(err)
	must(tx.Rollback())
	var rolledBack int
	must(db.QueryRow("SELECT count(*) FROM go_items WHERE name = 'rolled back'").Scan(&rolledBack))

	_, duplicate := db.Exec("INSERT INTO go_items (id, name) VALUES (?, 'duplicate')", id)

	out, err := json.Marshal(map[string]any{
		"name":       name,
		"data":       data,
		"rolledBack": rolledBack,
		"duplicate":  duplicate != nil,
		"redacted":   !strings.Contains(fmt.Sprintf("%v %#v", main, main), main.AuthToken),
	})
	must(err)
	fmt.Println(string(out))
}
