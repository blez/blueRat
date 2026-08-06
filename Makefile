BIN     := bluerat
PREFIX  ?= $(HOME)/.local

.PHONY: build release run check test fmt fmt-check clippy install uninstall clean

build:
	cargo build

release:
	cargo build --release

run:
	cargo run --release

check:
	cargo check --all-targets

test:
	cargo test

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --all-targets -- -D warnings

install: release
	install -Dm755 target/release/$(BIN) $(PREFIX)/bin/$(BIN)
	install -Dm644 assets/icon-256.png $(PREFIX)/share/icons/hicolor/256x256/apps/$(BIN).png
	install -Dm644 $(BIN).desktop $(PREFIX)/share/applications/$(BIN).desktop
	rm -f $(PREFIX)/share/icons/hicolor/1024x1024/apps/$(BIN).png  # legacy path from old installs

uninstall:
	rm -f $(PREFIX)/bin/$(BIN) \
	      $(PREFIX)/share/icons/hicolor/256x256/apps/$(BIN).png \
	      $(PREFIX)/share/icons/hicolor/1024x1024/apps/$(BIN).png \
	      $(PREFIX)/share/applications/$(BIN).desktop

clean:
	cargo clean
