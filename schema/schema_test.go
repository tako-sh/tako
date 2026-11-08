package schema

import (
	"bytes"
	"encoding/json"
	"strings"
	"testing"
)

type node struct {
	ID   uint64  `json:"id,string"`
	Note *string `json:"note,omitempty"`
	Next *node   `json:"next,omitempty"`
}

func TestGoWireSchema(t *testing.T) {
	app := New("mail")
	Handler[node, node](app, "lookup")
	var data bytes.Buffer
	if err := app.Write(&data); err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err := json.Unmarshal(data.Bytes(), &doc); err != nil {
		t.Fatal(err)
	}
	fields := doc["$defs"].(map[string]any)["node"].(map[string]any)
	props := fields["properties"].(map[string]any)
	if props["id"].(map[string]any)["type"] != "string" {
		t.Fatal(props)
	}
	if len(fields["required"].([]any)) != 1 {
		t.Fatal(fields)
	}
	if !strings.Contains(data.String(), "#/$defs/node") {
		t.Fatal(data.String())
	}
}
func TestUnsafeIntegerFails(t *testing.T) {
	app := New("mail")
	Handler[struct{ ID int64 }, bool](app, "lookup")
	if err := app.Write(&bytes.Buffer{}); err == nil || !strings.Contains(err.Error(), "safe integer") {
		t.Fatal(err)
	}
}

type recursiveList []recursiveList

func TestRecursiveCollectionsAndEmptyArrays(t *testing.T) {
	app := New("mail")
	Handler[recursiveList, [0]string](app, "lookup")
	var data bytes.Buffer
	if err := app.Write(&data); err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err := json.Unmarshal(data.Bytes(), &doc); err != nil {
		t.Fatal(err)
	}
	list := doc["$defs"].(map[string]any)["recursiveList"].(map[string]any)
	items := list["anyOf"].([]any)[0].(map[string]any)["items"].(map[string]any)
	if items["$ref"] != "#/$defs/recursiveList" {
		t.Fatal(items)
	}
	result := doc["x-tako"].(map[string]any)["primitives"].([]any)[0].(map[string]any)["result"].(map[string]any)
	if _, exists := result["prefixItems"]; exists || result["maxItems"] != float64(0) {
		t.Fatal(result)
	}
}

type textValue struct{}

func (textValue) MarshalText() ([]byte, error) { return []byte("value"), nil }

func TestCustomTextMarshalingRequiresDTO(t *testing.T) {
	app := New("mail")
	Handler[textValue, bool](app, "lookup")
	if err := app.Write(&bytes.Buffer{}); err == nil || !strings.Contains(err.Error(), "custom JSON marshaling") {
		t.Fatal(err)
	}
}
