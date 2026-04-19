# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Fork context

This repo is a fork of upstream `feschber/lan-mouse`. Focus of the fork is **user-facing polish** — enhanced connectivity, easier setup, Windows packaging — rather than mirroring upstream feature-for-feature.

- `origin` → `paalcyberbook/lan-mouse` (the fork — PRs land here)
- `upstream` → `feschber/lan-mouse` (pull-only, for syncing)
- Active work branch: `feature/enhanced-connectivity`

When suggesting changes, prefer UX improvements and connectivity/packaging fixes over speculative refactors. Don't assume upstream parity is a goal.

## Agent conventions

Read [AGENTS.md](AGENTS.md) for the full conventions — architecture (pipeline, terminology, async patterns), feature/cfg discipline, and workflow. The below is a Claude-specific supplement, not a replacement.

## Commands

The repo has a `Makefile` that wraps the most common `cargo` invocations. Prefer it over raw cargo when a target exists:

```sh
make            # fast release build (default) — parallel codegen + thin LTO
make build      # debug build
make release    # full release build (single codegen unit + fat LTO — slow)
make check      # cargo check only
make test       # cargo test --workspace
make clippy     # cargo clippy --workspace --all-targets --all-features -- -D warnings
make fmt        # cargo fmt --all
make fmt-check  # format check for CI
make install    # install binary, icon, desktop entry to /usr/local
make windows    # cross-compile for Windows via Docker (see Dockerfile.windows)
```

Single-crate work still uses cargo directly:

```sh
cargo build -p <crate>
cargo test -p <crate> -- <test_name>
RUST_LOG=lan_mouse=debug cargo run      # or LAN_MOUSE_LOG_LEVEL=debug
```

Run everything from the repo root — no `cd` in scripts.

### Pre-commit hook

`.githooks/pre-commit` runs fmt+clippy+test and blocks commits that fail. Enable once per clone with `git config core.hooksPath .githooks`. Never bypass it with `--no-verify` unless the user explicitly asks.

## Architecture at a glance

Rust workspace. Binary crate at repo root (`src/`) wires everything together; the pipeline lives in these workspace members:

- **`input-capture/`** — OS-specific capture backends (libei, layer-shell, x11, windows, macos). Produces a `Stream<CaptureEvent>`. Backends are tried in priority order.
- **`input-emulation/`** — OS-specific emulation backends. Implements the `Emulation` trait and maintains `pressed_keys` to release on disconnect.
- **`input-event/`** — Shared scancode enums and abstract event types. Extend here; don't duplicate translations in backends.
- **`lan-mouse-proto/`** — Wire format. Events are UDP, connection requests are TCP on the same port. **Bump the protocol version when serialization changes.**
- **`lan-mouse-ipc/`** — Local IPC between the service and frontends (GTK, CLI).
- **`lan-mouse-cli/`** — `lan-mouse cli …` subcommands.
- **`lan-mouse-gtk/`** — GTK4 + libadwaita frontend (optional; gated by the `gtk` feature).
- **`lan-mouse-launcher/`** — Platform-specific launcher helpers.

Top-level `src/` is the service/daemon: `service.rs` is the entry point, `capture.rs`/`emulation.rs` wire the workspace crates, `connect.rs`/`listen.rs` handle networking, `crypto.rs` handles DTLS via `webrtc-dtls`, `discovery.rs` handles mDNS. `main.rs` decides between GTK frontend, daemon, CLI, and capture/emulation test subcommands.

### Core invariant: clients are active XOR inactive

Each remote client is either *receiving* events or *sending* them back — never both at once. This prevents feedback loops. See [DOC.md](DOC.md#device-state---active-and-inactive). Any new feature that touches event routing must preserve this.

## Feature flags & conditional compilation

Features live in the root [`Cargo.toml`](Cargo.toml). The default set enables GTK + all Linux capture/emulation backends + discovery + clipboard; on non-Linux targets the unsupported backends are cfg-gated out automatically.

Gate OS-specific modules at the **module level**, not per-function, and use tight cfgs:

```rust
#[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
mod layer_shell;
```

New backends: add a feature to `Cargo.toml`, create a gated module, and log the backend selection so users can tell which one is active.

## Async conventions

Single-threaded tokio runtime (`Builder::new_current_thread`) driving a `LocalSet`. `futures` streams + `async_trait`. Model new flows as streams or async methods rather than threads. `InputCapture` implements `Stream` manually — don't short-circuit its pumping logic. If you truly need blocking work, use `spawn_blocking`.

## When working on OS-specific code

Backends for Linux, Windows, and macOS diverge significantly — clarify with the user which target they're on before changing capture/emulation code. If you can't test on the target OS, say so explicitly and document manual verification steps instead of claiming the change works.

## Configuration

Config file: `$XDG_CONFIG_HOME/lan-mouse/config.toml` (defaults to `~/.config/lan-mouse/config.toml`). Example in [`config.toml`](config.toml) and in the README. Release-bind key symbols come from [`input-event/src/scancode.rs`](input-event/src/scancode.rs).

## Docs stay current

When changing public APIs or platform support, update [README.md](README.md) and/or [DOC.md](DOC.md) in the same PR.
