CARGO = cargo
DOCKER = docker
INSTALL_DIR = /usr/local/bin
ICON_DIR = /usr/local/share/icons/hicolor/scalable/apps
DESKTOP_DIR = /usr/local/share/applications
JOBS = $(shell nproc)

# Release profile with parallel codegen for faster builds
FAST_RELEASE_FLAGS = --config 'profile.release.codegen-units=16' --config 'profile.release.lto="thin"'

.PHONY: all build release release-fast check test clippy fmt clean install uninstall windows

## Default: fast release build
all: release-fast

## Debug build (all cores, fast)
build:
	$(CARGO) build -j $(JOBS)

## Full release build (optimized binary, single codegen unit + fat LTO)
release:
	$(CARGO) build --release -j $(JOBS)

## Fast release build (parallel codegen + thin LTO, slightly less optimized)
release-fast:
	$(CARGO) build --release -j $(JOBS) $(FAST_RELEASE_FLAGS)

## Type-check without building
check:
	$(CARGO) check -j $(JOBS)

## Run tests
test:
	$(CARGO) test --workspace -j $(JOBS)

## Run clippy lints
clippy:
	$(CARGO) clippy --workspace --all-targets --all-features -- -D warnings

## Format code
fmt:
	$(CARGO) fmt --all

## Format check (CI)
fmt-check:
	$(CARGO) fmt --all -- --check

## Clean build artifacts
clean:
	$(CARGO) clean

## Install to system
install: release-fast
	sudo cp target/release/lan-mouse $(INSTALL_DIR)/
	sudo mkdir -p $(ICON_DIR)
	sudo cp lan-mouse-gtk/resources/de.feschber.LanMouse.svg $(ICON_DIR)/
	sudo gtk-update-icon-cache $(dir $(ICON_DIR)) 2>/dev/null || true
	sudo mkdir -p $(DESKTOP_DIR)
	sudo cp de.feschber.LanMouse.desktop $(DESKTOP_DIR)/ 2>/dev/null || true
	sudo update-desktop-database $(DESKTOP_DIR) 2>/dev/null || true

## Uninstall from system
uninstall:
	sudo rm -f $(INSTALL_DIR)/lan-mouse
	sudo rm -f $(ICON_DIR)/de.feschber.LanMouse.svg
	sudo rm -f $(DESKTOP_DIR)/de.feschber.LanMouse.desktop

## Cross-compile for Windows via Docker (builds toolchain image once, reuses it)
windows:
	bash scripts/build-windows.sh

## Force rebuild the Windows toolchain image
windows-rebuild:
	bash scripts/build-windows.sh --rebuild

## Remove the Windows toolchain image
windows-clean:
	bash scripts/build-windows.sh --clean

## Show help
help:
	@echo "lan-mouse build targets:"
	@echo ""
	@echo "  make              Fast release build (default)"
	@echo "  make build        Debug build"
	@echo "  make release      Full optimized release build (slow)"
	@echo "  make release-fast Fast release build (parallel LTO)"
	@echo "  make check        Type-check only"
	@echo "  make test         Run all tests"
	@echo "  make clippy       Run linter"
	@echo "  make fmt          Format code"
	@echo "  make clean        Clean build artifacts"
	@echo "  make install      Build and install to $(INSTALL_DIR)"
	@echo "  make uninstall    Remove from system"
	@echo "  make windows      Cross-compile for Windows (reuses toolchain image)"
	@echo "  make windows-rebuild  Rebuild Windows toolchain from scratch"
	@echo "  make windows-clean    Remove Windows toolchain image"
	@echo "  make help         Show this help"
