.PHONY: init check verify fmt fmt-check clippy test guard-test deny machete coverage wasm-check run build \
	local-up local-down local-logs local-key local-memory launch usage

# ---- the local stack: LiteLLM and Postgres in containers, the memory root on disk.
# Reads local/.env. See local/README.md.
COMPOSE ?= podman compose
LOCAL_ENV = local/.env

local-up:
	@test -f $(LOCAL_ENV) || { echo "copy local/.env.example to local/.env and fill it in"; exit 1; }
	$(COMPOSE) --env-file $(LOCAL_ENV) -f local/compose.yaml up -d
	@echo "LiteLLM on http://127.0.0.1:4000; next: make local-key"

# LiteLLM shares Postgres's network namespace, so it goes first or down trips over it.
local-down:
	$(COMPOSE) --env-file $(LOCAL_ENV) -f local/compose.yaml rm -sf litellm
	$(COMPOSE) --env-file $(LOCAL_ENV) -f local/compose.yaml down

local-logs:
	$(COMPOSE) --env-file $(LOCAL_ENV) -f local/compose.yaml logs -f litellm

local-key:
	sh scripts/local-key.sh

# Run a harness confined, with the memory server attached and model calls through the
# egress. HARNESS=claude|codex, AUTH=subscription|key|gateway, ARGS passed to the harness.
#   make launch                                  claude on your own login
#   make launch HARNESS=codex AUTH=key           codex with OPENAI_API_KEY from local/.env
#   make launch AUTH=gateway                     claude through the local LiteLLM
HARNESS ?= claude
AUTH ?= subscription
PROVIDER ?=
launch: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/memory --root "$$MEMORY_ROOT" launch --harness $(HARNESS) --auth $(AUTH) \
		$(if $(PROVIDER),--provider $(PROVIDER),) -- $(ARGS)

usage:
	@set -a; . ./$(LOCAL_ENV); set +a; target/release/memory --root "$$MEMORY_ROOT" usage

local-memory:
	@set -a; . ./$(LOCAL_ENV); set +a; \
	if [ -f "$$MEMORY_ROOT/grants.toml" ]; then echo "memory root at $$MEMORY_ROOT"; \
	else target/release/memory --root "$$MEMORY_ROOT" init --domain dom-me; fi

# `make check` is the pre-commit gate: fast, and what every commit must pass.
check: fmt-check clippy test guard-test

# `make verify` is what CI runs: the gate plus dependencies, coverage, and the wasm build.
verify: check deny machete coverage wasm-check

init:
	git config core.hooksPath .githooks
	@echo "pre-commit hook installed: .githooks/pre-commit runs make check"

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

guard-test:
	sh .claude/hooks/guard-bash.test.sh

# Advisories, licences, duplicate versions, sources. Configured in deny.toml.
deny:
	cargo deny check

# Dependencies declared and never used.
machete:
	cargo machete

# Line coverage over the workspace. COVERAGE_MIN is a ratchet: it may be raised when
# coverage grows and may not be lowered to make a reading pass. Change it in the commit
# that earns it.
COVERAGE_MIN ?= 90
# crates/sandbox/src/linux.rs runs between fork and exec, where a profile is never written,
# so it is measured by the confined launch test rather than by line coverage.
COVERAGE_IGNORE = 'sandbox/src/linux\.rs'
coverage:
	cargo llvm-cov --workspace --ignore-filename-regex $(COVERAGE_IGNORE) --fail-under-lines $(COVERAGE_MIN) --lcov --output-path target/lcov.info
	cargo llvm-cov report --ignore-filename-regex $(COVERAGE_IGNORE) --summary-only

# The core crate must build for the browser target, or the reader and the plugins cannot.
wasm-check:
	cargo check -p dm-core --target wasm32-unknown-unknown

build:
	cargo build --release

run:
	cargo run -q -p memory -- $(ARGS)
