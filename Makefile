BIN     := wn-server
VERSION ?= $(shell git describe --tags --always --dirty 2>/dev/null || echo dev)
LDFLAGS := -X github.com/waxnerdhq/wn-server/internal/cli.Version=$(VERSION)

.PHONY: build test lint fmt check bundle clean

build:
	go build -ldflags "$(LDFLAGS)" -o bin/$(BIN) ./cmd/wn-server

test:
	go test ./...

lint:
	go vet ./...
	@command -v golangci-lint >/dev/null && golangci-lint run ./... || echo "golangci-lint not installed; ran go vet only"

fmt:
	gofmt -l -w .

check: fmt lint test build

# Linux x86_64 release tarball in dist/. Runs on Linux only.
bundle:
	VERSION=$(VERSION) bundle/build.sh dist

clean:
	rm -rf bin dist
