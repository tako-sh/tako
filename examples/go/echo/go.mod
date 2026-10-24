module go-echo-example

go 1.25.0

require (
	github.com/labstack/echo/v5 v5.4.1
	tako.sh v0.0.0
)

require golang.org/x/time v0.15.0 // indirect

replace tako.sh => ../../..
