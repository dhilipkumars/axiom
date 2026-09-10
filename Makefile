# Axiom developer entry points. See docs/PLAN.md for the phase gates.
SHELL := /usr/bin/env bash
GOBIN ?= $(shell go env GOPATH)/bin
BUF ?= $(GOBIN)/buf
GOLANGCI_LINT ?= $(GOBIN)/golangci-lint
# Postgres major to build/test the extension against locally (a `cargo pgrx init`-ed one).
PG ?= pg14
COMPOSE := docker compose -f deploy/compose/docker-compose.yml

.PHONY: all proto proto-check gateway-build gateway-test gateway-lint gateway-vuln \
        ext-build ext-test ext-lint ext-fmt ext-audit unit lint up down e2e-phase0 e2e

all: lint unit

## Protobuf
proto:
	cd proto && $(BUF) lint && $(BUF) generate

proto-check: proto
	git diff --exit-code -- gateway/gen

## Gateway (Go)
gateway-build:
	cd gateway && go build ./...

gateway-test:
	cd gateway && go test -race -count=1 ./...

gateway-lint:
	cd gateway && $(GOLANGCI_LINT) run ./...

gateway-vuln:
	cd gateway && go run golang.org/x/vuln/cmd/govulncheck@latest ./...

## Extension (Rust / pgrx)
ext-build:
	cd extension && cargo build --no-default-features --features $(PG)

ext-test:
	cd extension && cargo pgrx test $(PG)

ext-lint:
	cd extension && cargo fmt --check
	cd extension && cargo clippy --no-default-features --features $(PG) --all-targets -- -D warnings

ext-fmt:
	cd extension && cargo fmt

ext-audit:
	cd extension && cargo audit
	cd extension && cargo deny check

## Aggregates
unit: gateway-test ext-test
lint: gateway-lint ext-lint

## Local stack / E2E
up:
	$(COMPOSE) up -d --build --wait

down:
	$(COMPOSE) down -v --remove-orphans

e2e-phase0:
	./e2e/phase0.sh

e2e: e2e-phase0
