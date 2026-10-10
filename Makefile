# Faster builds when the tools are there: sccache caches compiled crates across clean
# builds and worktrees, nextest runs tests in parallel processes. Neither is required.
SCCACHE := $(shell command -v sccache 2>/dev/null)
ifneq ($(SCCACHE),)
export RUSTC_WRAPPER := $(SCCACHE)
endif
NEXTEST := $(shell cargo nextest --version 2>/dev/null)
TEST_CMD := $(if $(NEXTEST),cargo nextest run --workspace --no-fail-fast,cargo test --workspace)

.PHONY: e2e hosted install init check verify fmt fmt-check clippy test guard-test docs-only-test conformance-test conformance deny machete coverage wasm-check run build wslcsdk-test wslcsdk unsafe-check windows-target \
	local-up local-down local-logs local-key local-memory launch usage golden drift taps-check taps-build wasip2-target reader

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
#   make launch HARNESS=claude                   claude on your own login
#   make launch HARNESS=codex AUTH=key           codex with OPENAI_API_KEY from local/.env
#   make launch HARNESS=claude AUTH=gateway      claude through the gateway
# No default harness: say which one, so a run is never the wrong harness by omission.
HARNESS ?=
AUTH ?= subscription
PROVIDER ?=
# The memory's own calls (the recall deputy, mining) on a credential of their own, so a
# harness on a personal login leaves them to the gateway: SERVER_AUTH=gateway (or key,
# with SERVER_PROVIDER naming where).
SERVER_AUTH ?=
SERVER_PROVIDER ?=
SERVER_FLAGS = $(if $(SERVER_AUTH),--server-auth $(SERVER_AUTH),) $(if $(SERVER_PROVIDER),--server-provider $(SERVER_PROVIDER),)
PROMPT ?=
LOOP ?=
BACKEND ?= auto
launch: build
	@test -n "$(HARNESS)" || { echo "HARNESS= is required: claude, codex, gemini, antigravity, qwen, grok, muse, pi, copilot, opencode or command" >&2; exit 2; }
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/amem --root "$$MEMORY_ROOT" launch --harness $(HARNESS) --auth $(AUTH) \
		--backend $(BACKEND) $(if $(PROVIDER),--provider $(PROVIDER),) $(SERVER_FLAGS) $(if $(MODEL),--model $(MODEL),) \
		$(if $(SUBAGENTS),--subagents $(SUBAGENTS),) $(if $(PROMPT),--prompt '$(PROMPT)',) -- $(ARGS)

# The harness image, for the wslc backend (and any OCI runtime): the release binary is
# copied in beside the Dockerfile, then built with wslc on Windows/WSL or podman elsewhere.
IMAGE ?= durable-memory/harness:dev
image: build
	@cp target/release/amem images/harness/memory
	@if command -v wslc.exe >/dev/null 2>&1; then \
		wslc.exe build -t $(IMAGE) "$$(wslpath -w images/harness)"; \
	elif command -v wslc >/dev/null 2>&1; then wslc build -t $(IMAGE) images/harness; \
	else podman build -t $(IMAGE) images/harness; fi

# Unpack the harness image under the record, for the Linux backend to pivot into. After
# this, `make launch` on Linux runs in the image by default.
unpack: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	target/release/amem --root "$$MEMORY_ROOT" image unpack --image $(IMAGE)

# The web launcher, opened in the browser, with local/.env and the virtual key in its
# environment. Outside this checkout, `amem` alone does this from credentials.env.
web: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/amem --root "$$MEMORY_ROOT" web $(if $(HARNESS),--harness $(HARNESS),) --auth $(AUTH) --backend $(BACKEND)

# One curation cycle on the record: the survey filed, the checker on a sample through
# the gateway, the canaries checked, the telemetry's alarms filed. EVERY=<seconds> loops.
curate: build
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	target/release/amem --root "$$MEMORY_ROOT" curate --auth $(AUTH) $(if $(MODEL),--model $(MODEL),) \
		--contradictions $(if $(EVERY),--every $(EVERY),)

# Vendor drift: the probe set (SET, default probes/drift.toml) run against MODEL through
# the gateway (or AUTH=key with PROVIDER), appended under state/drift, then the latest
# run checked against the first. FILE=1 files confirmed drift as a task for the owner.
#   make drift MODEL=qwen3.8-27b
DRIFT_AUTH = $(if $(filter subscription,$(AUTH)),gateway,$(AUTH))
DRIFT_PROVIDER = $(or $(PROVIDER),$(if $(filter gateway,$(DRIFT_AUTH)),llmao,openai))
drift: build
	@test -n "$(MODEL)" || { echo "MODEL= is required: the model whose drift is watched; amem models --auth gateway lists them" >&2; exit 2; }
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	target/release/amem --root "$$MEMORY_ROOT" drift probe --auth $(DRIFT_AUTH) --provider $(DRIFT_PROVIDER) \
		--model $(MODEL) $(if $(SET),--set $(SET),) $(if $(REPEAT),--repeat $(REPEAT),) >/dev/null && \
	target/release/amem --root "$$MEMORY_ROOT" drift check --provider $(DRIFT_PROVIDER) --model $(MODEL) \
		$(if $(BASELINE),--baseline $(BASELINE),) $(if $(FILE),--file,)

# Claim the next task from the ledger and run the harness on it. LOOP=1 keeps claiming.
work: build
	@test -n "$(HARNESS)" || { echo "HARNESS= is required: claude, codex, gemini, antigravity, qwen, grok, muse, pi, copilot, opencode or command" >&2; exit 2; }
	@set -a; . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$$(cat local/.virtual-key 2>/dev/null)" \
	GH_TOKEN="$${GH_TOKEN:-$$(gh auth token 2>/dev/null)}" \
	target/release/amem --root "$$MEMORY_ROOT" work --harness $(HARNESS) --auth $(AUTH) \
		--backend $(BACKEND) $(if $(PROVIDER),--provider $(PROVIDER),) $(SERVER_FLAGS) $(if $(MODEL),--model $(MODEL),) \
		$(if $(SUBAGENTS),--subagents $(SUBAGENTS),) $(if $(LOOP),--loop,)

# The weekend demo in a record of its own: DEMO_ROOT (default ~/memory-demo) seeded from
# DEMO_FROM (Markdown directories), one task worked by the harness, the savings measured.
DEMO_ROOT ?= $(HOME)/memory-demo
DEMO_FROM ?= docs
demo: build
	@test -n "$(HARNESS)" || { echo "HARNESS= is required: claude, codex, gemini, antigravity, qwen, grok, muse, pi, copilot or opencode" >&2; exit 2; }
	@set -a; [ -f $(LOCAL_ENV) ] && . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$${LLMAO_KEY:-$$(cat local/.virtual-key 2>/dev/null)}" \
	scripts/demo.sh "$(DEMO_ROOT)" $(DEMO_FROM)

# The paper's read-path claims measured on this record.
savings: build
	@set -a; . ./$(LOCAL_ENV); set +a; target/release/amem --root "$$MEMORY_ROOT" savings $(if $(QUERY),--query '$(QUERY)',)

# A golden suite scored against a record: SUITE (default golden/docs.toml, which fits the
# demo corpus) on ROOT (default DEMO_ROOT). PROVIDER adds the deputy's brief to the score.
SUITE ?= golden/docs.toml
GOLDEN_AUTH ?= key
golden: build
	@set -a; [ -f $(LOCAL_ENV) ] && . ./$(LOCAL_ENV); set +a; \
	LLMAO_KEY="$${LLMAO_KEY:-$$(cat local/.virtual-key 2>/dev/null)}" \
	target/release/amem --root "$${ROOT:-$(DEMO_ROOT)}" golden $(SUITE) $(if $(PROVIDER),--provider $(PROVIDER) --auth $(GOLDEN_AUTH),)

usage:
	@set -a; . ./$(LOCAL_ENV); set +a; target/release/amem --root "$$MEMORY_ROOT" usage

local-memory:
	@set -a; . ./$(LOCAL_ENV); set +a; \
	if [ -f "$$MEMORY_ROOT/grants.toml" ]; then echo "memory root at $$MEMORY_ROOT"; \
	else target/release/amem --root "$$MEMORY_ROOT" init --domain dom-me; fi

# `make check` is the pre-commit gate: fast, and what every commit must pass.
check: wasip2-target fmt-check clippy test guard-test docs-only-test wslcsdk-test conformance-test unsafe-check taps-check

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
	$(TEST_CMD)
guard-test:
	sh .claude/hooks/guard-bash.test.sh
docs-only-test:
	sh scripts/docs-only.test.sh
wslcsdk-test:
	sh scripts/fetch-wslcsdk.test.sh
conformance-test:
	sh scripts/conformance.test.sh

# The seven M0 steps against a server exposing the surface: a fresh record served by this
# build, or URL=<mcp endpoint> for another server, with OPERATOR=<command> taking
# `export <dir>`, `import <dir>` and `reindex` for steps 6 and 7. The default run names
# this build's own index file so step 7 shows the rebuild from a lost one.
conformance: build
	@if [ -n "$(URL)" ]; then \
		python3 scripts/conformance.py --url "$(URL)" $(if $(OPERATOR),--operator '$(OPERATOR)',); \
	else \
		root="$$(mktemp -d)/memory"; \
		target/release/amem --root "$$root" init --domain dom-conf >/dev/null; \
		python3 scripts/conformance.py --stdio "target/release/amem --root $$root serve" \
			--operator "target/release/amem --root $$root" \
			--drop-index "python3 -c \"import os; os.remove('$$root/state/index.db')\""; \
	fi

# `unsafe` only where AGENTS.md allows it, and every block there documented: the C API
# binding is Windows only, so its clippy runs for the Windows target, which needs no
# linker to check.
unsafe-check: windows-target
	sh scripts/unsafe-crates.sh
	cargo clippy -p dm-wslc-api --target x86_64-pc-windows-gnu --all-targets -- -D warnings

windows-target:
	@rustup target list --installed 2>/dev/null | grep -q '^x86_64-pc-windows-gnu$$' || \
		{ echo "the C API binding is linted for Windows; run: rustup target add x86_64-pc-windows-gnu" >&2; exit 1; }

# wslcsdk.dll, the WSL containers C API, beside the Windows memory.exe: pinned and checked.
wslcsdk:
	sh scripts/fetch-wslcsdk.sh "$(or $(DEST),target/release)"

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
# crates/wslc-api binds a Windows-only DLL; its layout test runs everywhere
# and the rest only where the DLL is.
# crates/taps/src/bindings.rs is generated by wasmtime's bindgen from wit/tap.wit.
COVERAGE_IGNORE = 'sandbox/src/linux\.rs|memory/src/wslc\.rs|wslc-api/src/lib\.rs|taps/src/bindings\.rs'
coverage:
	cargo llvm-cov --workspace --ignore-filename-regex $(COVERAGE_IGNORE) --fail-under-lines $(COVERAGE_MIN) --lcov --output-path target/lcov.info
	cargo llvm-cov report --ignore-filename-regex $(COVERAGE_IGNORE) --summary-only

# The browser reader's core: dm-reader built for the browser and bound to JavaScript with
# wasm-bindgen, into crates/web/static/reader/, which is committed so the web crate embeds
# it without the tool. Rerun after a change to crates/reader or crates/core.
reader:
	@command -v wasm-bindgen >/dev/null 2>&1 || \
		{ echo "needs wasm-bindgen-cli: cargo install wasm-bindgen-cli --version 0.2.129" >&2; exit 1; }
	cargo build -p dm-reader --target wasm32-unknown-unknown --release
	wasm-bindgen --target web --no-typescript --out-dir crates/web/static/reader \
		target/wasm32-unknown-unknown/release/dm_reader.wasm

# The core crate must build for the browser target, or the reader and the plugins cannot.
wasm-check:
	cargo check -p dm-core --target wasm32-unknown-unknown
	cargo check -p dm-policy --target wasm32-unknown-unknown
	cargo check -p dm-reader --target wasm32-unknown-unknown

# The taps are a workspace of their own, built as wasm32-wasip2 components. The host's
# tests build them; the gate also formats and lints them.
taps-check: wasip2-target
	cargo fmt --manifest-path taps/Cargo.toml --all -- --check
	cargo clippy --manifest-path taps/Cargo.toml --target wasm32-wasip2 --all-targets -- -D warnings

# The taps need the component target; say so plainly instead of failing deep in rustc.
wasip2-target:
	@rustup target list --installed 2>/dev/null | grep -q '^wasm32-wasip2$$' || \
		{ echo "the taps build for wasm32-wasip2; run: rustup target add wasm32-wasip2" >&2; exit 1; }

taps-build:
	cargo build --manifest-path taps/Cargo.toml --target wasm32-wasip2 --release

build:
	cargo build --release

# The browser tests, in the Playwright image: the launch form, a session's terminal and
# pane, the reader, the token, every page at three widths in both themes. Not part of
# `check` or `verify`; run it for a change to the pages. FILTER= narrows to a spec or -g.
e2e:
	sh scripts/e2e.sh $(FILTER)

# The pages as plain files in target/hosted, for a static host; see "A hosted page" in
# local/README.md. `amem --hosted <url>` opens that copy paired with this machine's server.
hosted:
	sh scripts/hosted.sh

# `amem` on the PATH, from this checkout.
install:
	cargo install --path crates/memory --locked

run:
	cargo run -q -p amem -- $(ARGS)
