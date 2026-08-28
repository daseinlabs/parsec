# Thin task runner — targets delegate to scripts/; cargo owns real builds.
# Keep recipes single-line: stock macOS make is 3.81 (no .ONESHELL).
.PHONY: help plugin test fmt clippy rust-test check dev dev-down dev-logs \
	platform-up platform-down platform-reset platform-logs release

help:
	@echo "make dev      — brain in docker + proxy on the host (manual testing)"
	@echo "make dev-down — stop the brain container"
	@echo "make dev-logs — tail the brain container logs"
	@echo "make platform-up    — local Postgres + platform API on :8080"
	@echo "make platform-down  — stop them (keeps the database)"
	@echo "make platform-reset — wipe the database, re-run migrations/*.sql"
	@echo "make plugin   — build + install the local (gitignored) plugin binary"
	@echo "make test     — run the full test suite (scripts/test_all.sh)"
	@echo "make check    — rust fmt + clippy + tests (single source for CI and test_all.sh)"
	@echo "make release VERSION=X.Y.Z — bump workspace version, commit, tag (push = publish)"

# Local loop: brain in a container, proxy as a HOST process (your Claude auth
# headers must never enter a container — docs/manual-testing.md).
dev:
	scripts/dev_up.sh

dev-down:
	docker compose down

dev-logs:
	docker compose logs -f brain

# Local control plane: Postgres (migrations auto-applied on first init) +
# the platform API. Config comes from .env.local — cp .env.local.example first.
platform-up:
	docker compose --profile platform up -d --build

platform-down:
	docker compose --profile platform down

# Migrations only run on an empty volume, so a schema change means a wipe.
platform-reset:
	docker compose --profile platform down -v && docker compose --profile platform up -d

platform-logs:
	docker compose --profile platform logs -f platform

# Rust static checks + tests. CI (.github/workflows/ci.yml) and
# scripts/test_all.sh both call these targets so the flags can't drift.
fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

rust-test:
	cargo test --workspace --quiet

check: fmt clippy rust-test

# Build + install the local (gitignored) plugin binary. BRAIN_URL bakes the
# default brain endpoint (brain.rs BAKED_BRAIN_URL); PLATFORM_URL bakes the
# ledger sink (ledger_ship.rs BAKED_PLATFORM_URL). Same two knobs release.yml
# stamps, so the plugin reaches both with zero runtime config; omit either for
# a dev build that reads PARSEC_BRAIN_URL / PARSEC_PLATFORM_URL at runtime.
#   make plugin
#   make plugin BRAIN_URL=http://127.0.0.1:8090 PLATFORM_URL=http://127.0.0.1:8080
#   make plugin BRAIN_URL=https://dasein-brain-939340394421.us-central1.run.app
plugin:
	PARSEC_DEFAULT_BRAIN_URL="$(BRAIN_URL)" PARSEC_DEFAULT_PLATFORM_URL="$(PLATFORM_URL)" scripts/refresh_plugin_bin.sh

test:
	scripts/test_all.sh

# Cut a release: bump the workspace version (what `parsec --version` reports),
# commit, tag. Pushing the tag is what triggers release.yml — left to you.
# Channel: X.Y.0 = stable (everyone), anything else = patch (opt-in) — see
# docs/release-channels.md.
release:
	VERSION="$(VERSION)" scripts/release.sh
