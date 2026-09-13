# Axiom developer entry points. See docs/PLAN.md for the phase gates.
SHELL := /usr/bin/env bash
GOBIN ?= $(shell go env GOPATH)/bin
BUF ?= $(GOBIN)/buf
GOLANGCI_LINT ?= $(GOBIN)/golangci-lint
# Postgres major to build/test the extension against locally (a `cargo pgrx init`-ed one).
# The supported window is 16 through the latest major; see issue #19.
PG ?= pg16
COMPOSE := docker compose -f deploy/compose/docker-compose.yml

.PHONY: all proto proto-check gateway-build gateway-test gateway-lint gateway-vuln \
        ext-build ext-test ext-lint ext-fmt ext-audit unit lint docs-generate docs-check up down e2e-ping e2e-phase0 e2e-pods e2e-phase1 e2e-configmaps e2e-phase2 e2e-watch e2e-phase3 e2e-crd e2e-phase4 e2e-cluster e2e-phase5 e2e

all: lint unit

## Protobuf
proto:
	cd proto && $(BUF) lint && $(BUF) generate

proto-check: proto
	# See docs-check for why --intent-to-add: a newly generated file is
	# untracked, and `git diff` would not report it.
	git add --intent-to-add gateway/gen
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
	# Always start from a fresh scratch cluster: a stale/partially-cached
	# test-pgdata makes pgrx skip initdb and then fail to start Postgres.
	rm -rf "$${CARGO_TARGET_DIR:-extension/target}/test-pgdata"
	cd extension && cargo pgrx test $(PG)

ext-lint:
	cd extension && cargo fmt --check
	cd extension && cargo clippy --no-default-features --features $(PG) --all-targets -- -D warnings

ext-fmt:
	cd extension && cargo fmt

ext-audit:
	cd extension && cargo audit
	cd extension && cargo deny check

## Documentation
# Regenerates the reference pages whose sole source of truth is code: the gRPC
# API, the gateway's flags, the FDW options and the column rules. Everything
# under docs/ that is *not* in docs/generated/ is written by hand.
docs-generate:
	cd gateway && go run ./cmd/docsgen -root .. -out ../docs/generated

# The anti-drift gate. Same shape as proto-check: regenerate, then fail if that
# changed anything that was committed. Deliberately not a rule like "every PR
# touching code must touch docs/" -- that false-positives on every refactor and
# is satisfied by a whitespace change (docs/PLAN.md Phase 6 Part 2).
docs-check: docs-generate
	# --intent-to-add first: `git diff` does not report untracked files, so
	# without it this passes when the generator emits a page nobody committed,
	# or when a generated page is deleted from the commit and recreated here.
	# Both are exactly the drift the gate exists to catch.
	git add --intent-to-add docs/generated
	git diff --exit-code -- docs/generated

## Aggregates
unit: gateway-test ext-test
lint: gateway-lint ext-lint

## Local stack / E2E
up:
	$(COMPOSE) up -d --build --wait

down:
	$(COMPOSE) down -v --remove-orphans

# Tests live in e2e/*_test.sh and share the setup library in e2e/lib/.
e2e-ping:
	./e2e/ping_test.sh

# PLAN.md name for the Phase 0 gate; the Ping test is that gate.
e2e-phase0: e2e-ping

# Needs kind + kubectl on PATH; creates/deletes a cluster named axiom-e2e.
e2e-pods:
	./e2e/pods_test.sh

# PLAN.md name for the Phase 1 gate; the Pods test is that gate.
e2e-phase1: e2e-pods

e2e-configmaps:
	./e2e/configmaps_test.sh

# PLAN.md name for the Phase 2 gate; the ConfigMaps test is that gate.
e2e-phase2: e2e-configmaps

e2e-watch:
	./e2e/watch_test.sh

# PLAN.md name for the Phase 3 gate; the watch test is that gate.
e2e-phase3: e2e-watch

e2e-crd:
	./e2e/crd_test.sh

# PLAN.md name for the Phase 4 gate; the CRD test is that gate.
e2e-phase4: e2e-crd

e2e-cluster:
	./e2e/cluster_test.sh

# PLAN.md name for the Phase 5 gate; the whole-cluster test is that gate.
e2e-phase5: e2e-cluster

# Every completed phase's gate, oldest first (regression order per RULES.md §4),
# sharing one image build and one kind cluster across all of them. The
# individual targets above still work standalone; this is what CI runs.
e2e:
	./e2e/run_all.sh
