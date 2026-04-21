# CleanMic Makefile
#
# Targets:
#   build     - Build release binary with all features
#   appimage  - Kill any running instance, build AppImage
#   kill      - Kill any running CleanMic instance
#   install   - Install binary, desktop file, and icons to system (PREFIX=/usr/local)
#   uninstall - Remove installed files
#   fmt       - Run cargo fmt
#   lint      - Run cargo clippy
#   test      - Run cargo test
#   clean     - Remove build artifacts

# Use cargo from PATH; fall back to $HOME/.cargo/bin if rustup is installed
CARGO   ?= $(shell command -v cargo 2>/dev/null || echo "$(HOME)/.cargo/bin/cargo")
PREFIX  ?= /usr/local
DESTDIR ?=

BINARY  := target/release/cleanmic

.PHONY: build appimage kill vendors mo install uninstall fmt lint test clean

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

clean:
	$(CARGO) clean
	rm -rf build/
	rm -f locale/fr/LC_MESSAGES/cleanmic.mo
