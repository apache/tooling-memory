.PHONY: init check fmt clippy test guard-test run build

# `make check` is the gate: what the pre-commit hook runs, and what CI runs.
check: fmt-check clippy test guard-test

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

build:
	cargo build --release

run:
	cargo run -q -p memory -- $(ARGS)
