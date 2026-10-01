.PHONY: init check verify fmt fmt-check clippy test guard-test deny machete coverage wasm-check run build

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
coverage:
	cargo llvm-cov --workspace --fail-under-lines $(COVERAGE_MIN) --lcov --output-path target/lcov.info
	cargo llvm-cov report --summary-only

# The core crate must build for the browser target, or the reader and the plugins cannot.
wasm-check:
	cargo check -p dm-core --target wasm32-unknown-unknown

build:
	cargo build --release

run:
	cargo run -q -p memory -- $(ARGS)
