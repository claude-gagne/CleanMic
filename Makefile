# CleanMic Makefile
#
# Targets:
#   build         - Build release binary with all features
#   appimage      - Kill any running instance, build AppImage
#   kill          - Kill any running CleanMic instance
#   install       - Install binary, desktop file, and icons to system (PREFIX=/usr/local)
#   uninstall     - Remove installed files
#   fmt           - Run cargo fmt
#   lint          - Run cargo clippy
#   test          - Run cargo test
#   ci-check      - Run fmt-check, clippy -D warnings, and tests (mirrors release CI)
#   harness-test  - Offline checks for the silent E2E test harness (scripts/e2e-audio.sh)
#   e2e-audio     - Run the silent E2E audio test (SCENARIOS="...", E2E_ARGS="...")
#   nested-run    - Start a nested CleanMic session by hand (NESTED_ARGS="...")
#   nested-stop   - Stop that nested session
#   clean         - Remove build artifacts

# Use cargo from PATH; fall back to $HOME/.cargo/bin if rustup is installed
CARGO   ?= $(shell command -v cargo 2>/dev/null || echo "$(HOME)/.cargo/bin/cargo")
PREFIX  ?= /usr/local
DESTDIR ?=

BINARY  := target/release/cleanmic

.PHONY: build appimage kill vendors mo install uninstall fmt lint test ci-check clean harness-test e2e-audio nested-run nested-stop

mo:
	@mkdir -p locale/fr/LC_MESSAGES
	msgfmt locale/fr/LC_MESSAGES/cleanmic.po -o locale/fr/LC_MESSAGES/cleanmic.mo

build: mo
	$(CARGO) build --release --all-features

# Kill any running instance before rebuilding the AppImage.
# The binary inside the AppImage fuse-mount is named "cleanmic" (lowercase).
kill:
	@pkill -x cleanmic 2>/dev/null && echo "Killed running cleanmic" || echo "No running cleanmic found"

vendors:
	bash scripts/fetch-vendors.sh

appimage: kill vendors build
	bash scripts/build-appimage.sh

install: build
	install -Dm755 $(BINARY)                                    $(DESTDIR)$(PREFIX)/bin/cleanmic
	install -Dm644 assets/com.cleanmic.CleanMic.desktop         $(DESTDIR)$(PREFIX)/share/applications/com.cleanmic.CleanMic.desktop
	install -Dm644 assets/icons/com.cleanmic.CleanMic.svg       $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/com.cleanmic.CleanMic.svg
	install -Dm644 assets/icons/cleanmic-active.svg             $(DESTDIR)$(PREFIX)/share/icons/hicolor/symbolic/apps/cleanmic-active-symbolic.svg
	install -Dm644 assets/icons/cleanmic-disabled.svg           $(DESTDIR)$(PREFIX)/share/icons/hicolor/symbolic/apps/cleanmic-disabled-symbolic.svg

uninstall:
	rm -f  $(DESTDIR)$(PREFIX)/bin/cleanmic
	rm -f  $(DESTDIR)$(PREFIX)/share/applications/com.cleanmic.CleanMic.desktop
	rm -f  $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/com.cleanmic.CleanMic.svg
	rm -f  $(DESTDIR)$(PREFIX)/share/icons/hicolor/symbolic/apps/cleanmic-active-symbolic.svg
	rm -f  $(DESTDIR)$(PREFIX)/share/icons/hicolor/symbolic/apps/cleanmic-disabled-symbolic.svg

fmt:
	$(CARGO) fmt

lint:
	$(CARGO) clippy --all-features

test:
	$(CARGO) test --all-features

# CI-mirror: runs the same gates as .github/workflows/release.yml in fail-fast order.
# Use this locally to confirm a change will pass CI before pushing.
ci-check:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-features -- -D warnings
	$(CARGO) test --all-features

clean:
	$(CARGO) clean
	rm -rf build/
	rm -f locale/fr/LC_MESSAGES/cleanmic.mo

# Offline checks for the silent E2E audio test harness itself: shell syntax
# (+ shellcheck when installed), the sandboxed nested-run.sh self-test
# (screen rule, tiny-scale guard, owner-process refusal/survival, bad-config
# guard), and the Python metric/report unit tests. No X server, no
# PipeWire, no network -- see scripts/e2e/README.md.
harness-test:
	@for f in scripts/nested-run.sh scripts/e2e-audio.sh scripts/test-nested-run.sh; do \
		bash -n $$f || exit 1; \
	done
	@if command -v shellcheck >/dev/null 2>&1; then \
		shellcheck -x scripts/nested-run.sh scripts/e2e-audio.sh scripts/test-nested-run.sh; \
	else \
		echo "shellcheck not installed -- bash -n only"; \
	fi
	bash scripts/test-nested-run.sh
	python3 -m pytest -q -p no:cacheprovider scripts/e2e

# Real, silent virtual-mic E2E audio test (see scripts/e2e/README.md).
# SCENARIOS defaults to "baseline"; e.g. SCENARIOS="baseline swaps toggle
# modes dc autogain" or SCENARIOS=all. E2E_ARGS passes through extra flags
# (--out, --lang, --monitor-null-sink, ...).
e2e-audio:
	bash scripts/e2e-audio.sh $(E2E_ARGS) $(or $(SCENARIOS),baseline)

# Manual nested-CleanMic poking: start a Xephyr display and launch into it.
# NESTED_ARGS passes through nested-run.sh launch options (--lang, --config
# 'KEY = VALUE', ...).
nested-run:
	bash scripts/nested-run.sh xephyr
	bash scripts/nested-run.sh launch $(NESTED_ARGS)

nested-stop:
	bash scripts/nested-run.sh stop
