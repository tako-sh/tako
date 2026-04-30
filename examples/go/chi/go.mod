module go-chi-example

go 1.22

require (
	github.com/go-chi/chi/v5 v5.2.5
	tako.sh v0.0.0
)

require github.com/santhosh-tekuri/jsonschema/v5 v5.3.1 // indirect

replace tako.sh => ../../..
