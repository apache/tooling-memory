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
PROMPT ?=
LOOP ?=
BACKEND ?= auto
launch: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/memory --root "$$MEMORY_ROOT" launch --harness $(HARNESS) --auth $(AUTH) \
		--backend $(BACKEND) $(if $(PROVIDER),--provider $(PROVIDER),) $(if $(PROMPT),--prompt '$(PROMPT)',) -- $(ARGS)

# The harness image, for the wslc backend (and any OCI runtime): the release binary is
# copied in beside the Dockerfile, then built with wslc on Windows/WSL or podman elsewhere.
IMAGE ?= durable-memory/harness:dev
image: build
	@cp target/release/memory images/harness/memory
	@if command -v wslc.exe >/dev/null 2>&1; then \
		wslc.exe build -t $(IMAGE) "$$(wslpath -w images/harness)"; \
	elif command -v wslc >/dev/null 2>&1; then wslc build -t $(IMAGE) images/harness; \
	else podman build -t $(IMAGE) images/harness; fi

# Unpack the harness image under the record, for the Linux backend to pivot into. After
# this, `make launch` on Linux runs in the image by default.
unpack: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	target/release/memory --root "$$MEMORY_ROOT" image unpack --image $(IMAGE)

# The web terminal on 127.0.0.1:4200: sessions start from the browser.
web: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/memory --root "$$MEMORY_ROOT" web --harness $(HARNESS) --auth $(AUTH) --backend $(BACKEND)

# Claim the next task from the ledger and run the harness on it. LOOP=1 keeps claiming.
work: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/memory --root "$$MEMORY_ROOT" work --harness $(HARNESS) --auth $(AUTH) \
		--backend $(BACKEND) $(if $(PROVIDER),--provider $(PROVIDER),) $(if $(LOOP),--loop,)

# The weekend demo in a record of its own: DEMO_ROOT (default ~/memory-demo) seeded from
# DEMO_FROM (Markdown directories), one task worked by the harness, the savings measured.
DEMO_ROOT ?= $(HOME)/memory-demo
DEMO_FROM ?= docs
demo: build
	@scripts/demo.sh "$(DEMO_ROOT)" $(DEMO_FROM)

# The paper's read-path claims measured on this record.
savings: build
	@set -a; . ./$(LOCAL_ENV); set +a; target/release/memory --root "$$MEMORY_ROOT" savings $(if $(QUERY),--query '$(QUERY)',)

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
# crates/memory/src/wslc.rs drives wslc.exe, which CI does not have; its argument builders
# are unit-tested and the rest is proven by hand on a WSL machine.
COVERAGE_IGNORE = 'sandbox/src/linux\.rs|memory/src/wslc\.rs'
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
