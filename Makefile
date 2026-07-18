# Thin task runner — targets delegate to scripts/; cargo owns real builds.
# Keep recipes single-line: stock macOS make is 3.81 (no .ONESHELL).
.PHONY: help plugin test fmt clippy rust-test check

help:
	@echo "make plugin  — build + install the local (gitignored) plugin binary"
	@echo "make test    — run the full test suite (scripts/test_all.sh)"
	@echo "make check   — rust fmt + clippy + tests (single source for CI and test_all.sh)"

# Rust static checks + tests. CI (.github/workflows/ci.yml) and
# scripts/test_all.sh both call these targets so the flags can't drift.
fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

rust-test:
	cargo test --workspace --quiet

check: fmt clippy rust-test

plugin:
	scripts/refresh_plugin_bin.sh

test:
	scripts/test_all.sh
