package tako

import (
	"bytes"
	"encoding/json"
	"errors"
)

// Optional distinguishes an absent JSON field from a present value. A nullable
// optional field uses Optional[*T]: Set=false is absent; Set=true, Value=nil is null.
// Generated object marshalers omit absent fields, including on Go 1.21.
type Optional[T any] struct {
	Set   bool
	Value T
}

// Null represents exactly JSON null.
type Null struct{}

func (Null) MarshalJSON() ([]byte, error) { return []byte("null"), nil }
func (*Null) UnmarshalJSON(data []byte) error {
	if !bytes.Equal(bytes.TrimSpace(data), []byte("null")) {
		return errors.New("expected null")
	}
	return nil
}

// Never represents an uninhabited JSON Schema.
type Never struct{}

func (Never) MarshalJSON() ([]byte, error) { return nil, errors.New("uninhabited schema") }
func (*Never) UnmarshalJSON([]byte) error  { return errors.New("uninhabited schema") }

// Array serializes an empty or nil slice as [] and rejects JSON null.
type Array[T any] []T

func (a Array[T]) MarshalJSON() ([]byte, error) {
	if a == nil {
		return []byte("[]"), nil
	}
	return json.Marshal([]T(a))
}
func (a *Array[T]) UnmarshalJSON(data []byte) error {
	if bytes.Equal(bytes.TrimSpace(data), []byte("null")) {
		return errors.New("expected array")
	}
	var value []T
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*a = value
	return nil
}
