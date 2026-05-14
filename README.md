# Lan Mouse+

> **Fork notice.** This is `paalcyberbook/lan-mouse` — a user-facing
> fork of [feschber/lan-mouse](https://github.com/feschber/lan-mouse)
> branded **Lan Mouse+**. It carries the full upstream feature set plus
> enhancements focused on connectivity and UX, notably:
>
> * File / folder transfer between machines (edge drop zone on Wayland
>   wlroots + Windows; per-client GTK drop target everywhere else).
> * Oversize clipboard + clipboard-image handoff via the file-transfer
>   channel.
> * Windows packaging improvements (`Dockerfile.windows`, installer).
>
> See the [File transfer](#file-transfer-edge-drop-zone) section below
> for what's new in this fork.

[![CI](https://github.com/feschber/lan-mouse/actions/workflows/rust.yml/badge.svg)](https://github.com/feschber/lan-mouse/actions/workflows/rust.yml) [![Cachix](https://github.com/feschber/lan-mouse/actions/workflows/cachix.yml/badge.svg)](https://github.com/feschber/lan-mouse/actions/workflows/cachix.yml) [![Release](https://github.com/feschber/lan-mouse/actions/workflows/release.yml/badge.svg)](https://github.com/feschber/lan-mouse/actions/workflows/release.yml)

[![crates.io](https://img.shields.io/crates/v/lan-mouse.svg)](https://crates.io/crates/lan-mouse)  [![license](https://img.shields.io/crates/l/lan-mouse.svg)](https://github.com/feschber/lan-mouse/blob/main/Cargo.toml)

Lan Mouse+ is a *cross-platform* mouse, keyboard, and file-sharing tool
similar to universal-control on Apple devices.
It allows for using multiple PCs via a single set of mouse and keyboard,
and for moving files between them without scp/USB stick detours.
This is also known as a Software KVM switch.

Goal of this project is to be an open-source alternative to proprietary tools like [Synergy 2/3](https://symless.com/synergy), [Share Mouse](https://www.sharemouse.com/de/)
and other open source tools like [Deskflow](https://github.com/deskflow/deskflow) or [Input Leap](https://github.com/input-leap) (Synergy fork).

Focus lies on performance, ease of use and a maintainable implementation that can be expanded to support additional backends for e.g. Android, iOS, ... in the future.

***blazingly fast™*** because it's written in rust.

- _Now with a gtk frontend_
- _Now with file transfer (fork)_

<picture>
    <source media="(prefers-color-scheme: dark)" srcset="/screenshots/dark.png?raw=true">
    <source media="(prefers-color-scheme: light)" srcset="/screenshots/light.png?raw=true">
    <img alt="Screenshot of Lan-Mouse" srcset="/screenshots/dark.png">
</picture>


## Encryption

Lan Mouse encrypts all network traffic using the DTLS implementation provided by [WebRTC.rs](https://github.com/webrtc-rs/webrtc).
There are currently no mitigations in place for timing side-channel attacks.

File transfer (see below) uses a separate QUIC connection with TLS 1.3 via
`rustls`, reusing the same self-signed cert and pinned-fingerprint trust
model as DTLS — no new authorization step.

## File transfer (edge drop-zone)

> [!Note]
> This is an enhancement on the `paalcyberbook/lan-mouse` fork, not yet
> upstream.

When two peers are connected, lan-mouse can move files and folders between
them in addition to input events. Three ways to send:

- **Edge drop** (Wayland wlroots — sway, Hyprland): drag a file from your
  file manager toward the screen edge that borders the remote client. A
  1px-wide layer-shell surface catches the drop and ships the file over
  QUIC.
- **Per-client drop target** (any GTK session): drag a file onto a client
  row in the lan-mouse window. Works on GNOME, KDE, and anywhere else the
  GTK frontend runs. Drop one file at a time.
- **Oversize clipboard handoff**: if you copy text larger than 64 KB or a
  clipboard image, lan-mouse prompts to route the payload through the
  file-transfer channel instead of truncating.

On the receiver, a GTK dialog appears for every incoming offer with the
sender's fingerprint, entry count, and total size. Accept opens a folder
picker (defaulting to `~/Downloads`, remembering your last choice in
`$XDG_STATE_HOME/lan-mouse/state.json`). The user must always confirm —
no silent writes.

### Transport

* Separate QUIC connection on `file_transfer_port` (default: main port + 1,
  i.e. `4243`).
* Per-file streams inside one QUIC connection; each chunk is compressed
  with zstd (level 3) except for already-compressed formats (PNG/JPEG/zip/…)
  where the magic-byte probe skips the wasted CPU.
* Each transferred file is verified with a BLAKE3 hash on the receiver;
  partial files land at `<name>.lanmouse.part` and are unlinked on error
  or cancellation.
* Zip-slip defense: `..` and absolute components in relative paths are
  rejected before the receiver opens the destination file.

### Configuration

All file-transfer settings are optional; sensible defaults apply.

```toml
# [config.toml]

# UDP port for the QUIC file-transfer channel (default: main port + 1)
file_transfer_port = 4243

# Reject incoming offers that exceed these caps (default: 64 GiB, 100 000
# entries). Prevents runaway transfers and limits the blast radius of a
# malicious sender.
max_file_transfer_bytes = 68719476736
max_file_transfer_entries = 100000

# Accept every incoming transfer without a user prompt, saving to the
# specified directory. Opt-in only; off by default. Requires both flags.
auto_accept_file_transfers = false
# auto_accept_dir = "/home/you/Incoming"
```

### Limitations

- Wayland KWin (Plasma 6): edge drop depends on KWin forwarding
  `wl_data_device` events to layer-shell surfaces. Unverified on current
  stable; if edge drops don't fire, use the per-client GTK drop target.
- GNOME: no `zwlr_layer_shell_v1`, so edge drop isn't available. Use the
  GTK drop target.
- Windows: edge drop is not yet implemented. Use the GTK drop target.
- X11: file transfer works (transport is OS-agnostic), but there is no
  edge-drop source yet.
- Multiple files at once: Wayland edge drops support multi-file offers;
  the GTK per-client drop target only accepts one file per drop.
- Daisy-chaining (A → B → C): a dropped file ends at the first hop. To
  forward it further, re-drop on the next machine.

## OS Support

Most current desktop environments and operating systems are fully supported, this includes
- GNOME >= 45
- KDE Plasma >= 6.1
- Most wlroots based compositors, including Sway (>= 1.8), Hyprland and Wayfire
- Windows
- MacOS


### Caveats / Known Issues

> [!Important]
> - **X11** currently only has support for input emulation, i.e. can only be used on the receiving end.
>
> - **Sway / wlroots**: Wlroots based compositors without libei support on the receiving end currently do not handle modifier events on the client side.
> This results in CTRL / SHIFT / ALT / SUPER keys not working with a sending device that is NOT using the `layer-shell` backend
>
> - **Wayfire**: If you are using [Wayfire](https://github.com/WayfireWM/wayfire), make sure to use a recent version (must be newer than October 23rd) and **add `shortcuts-inhibit` to the list of plugins in your wayfire config!**
> Otherwise input capture will not work.
>
> - **Windows**: The mouse cursor will be invisible when sending input to a Windows system if
> there is no real mouse connected to the machine.

For more detailed information about os support see [Detailed OS Support](#detailed-os-support)

### Android & IOS

A proof of concept for an Android / IOS Application by [rohitsangwan01](https://github.com/rohitsangwan01) can be found [here](https://github.com/rohitsangwan01/lan-mouse-mobile).
It can be used as a remote control for any device supported by Lan Mouse.

## Installation

<details>
    <summary>Arch Linux</summary>

Lan Mouse can be installed from the [official repositories](https://archlinux.org/packages/extra/x86_64/lan-mouse/):

```sh
pacman -S lan-mouse
```

The prerelease version (following `main`) is available on the AUR:

```sh
paru -S lan-mouse-git
```
</details>


<details>
    <summary>Nix (OS)</summary>

- nixpkgs: [search.nixos.org](https://search.nixos.org/packages?channel=unstable&show=lan-mouse&from=0&size=50&sort=relevance&type=packages&query=lan-mouse)
- flake: [README.md](./nix/README.md)
</details>

<details>
    <summary>Fedora</summary>
You can install Lan Mouse from the [Terra Repository](https://terra.fyralabs.com).


After enabling Terra:

```sh
dnf install lan-mouse
```
</details>

<details>
    <summary>MacOS</summary>

- Download the package for your Mac (Intel or ARM) from the releases page
- Unzip it
- Remove the quarantine with `xattr -rd com.apple.quarantine "Lan Mouse.app"`
- Launch the app
- Grant accessibility permissions in System Preferences

</details>


<details>
    <summary>Manual Installation</summary>

First make sure to [install the necessary dependencies](#installing-dependencies-for-development--compiling-from-source).

Precompiled release binaries for Windows, MacOS and Linux are available in the [releases section](https://github.com/feschber/lan-mouse/releases).
For Windows, the depenedencies are included in the .zip file, for other operating systems see [Installing Dependencies](#installing-dependencies-for-development--compiling-from-source).

Alternatively, the `lan-mouse` binary can be compiled from source (see below).

### Installing desktop file, app icon and firewall rules (optional)
```sh
# install lan-mouse (replace path/to/ with the correct path)
sudo cp path/to/lan-mouse /usr/local/bin/

# install app icon
sudo mkdir -p /usr/local/share/icons/hicolor/scalable/apps
sudo cp lan-mouse-gtk/resources/de.feschber.LanMouse.svg /usr/local/share/icons/hicolor/scalable/apps

# update icon cache
gtk-update-icon-cache /usr/local/share/icons/hicolor/

# install desktop entry
sudo mkdir -p /usr/local/share/applications
sudo cp de.feschber.LanMouse.desktop /usr/local/share/applications

# when using firewalld: install firewall rule
sudo cp firewall/lan-mouse.xml /etc/firewalld/services
# -> enable the service in firewalld settings
```

Instead of downloading from the releases, the `lan-mouse` binary
can be easily compiled via cargo or nix:

### Compiling and installing manually:
```sh
# compile in release mode
cargo build --release

# install lan-mouse
sudo cp target/release/lan-mouse /usr/local/bin/
```

### Compiling and installing via cargo:
```sh
# will end up in ~/.cargo/bin
cargo install lan-mouse
```

### Compiling and installing via nix:
```sh
# you can find the executable in result/bin/lan-mouse
nix-build
```
### Conditional compilation
Support for other platforms is omitted automatically based on the active
rust toolchain.

Additionally, available backends and frontends can be configured manually via
[cargo features](https://doc.rust-lang.org/cargo/reference/features.html).

E.g. if only support for sway is needed, the following command produces
an executable with support for only the `layer-shell` capture backend
and `wlroots` emulation backend:
```sh
cargo build --no-default-features --features layer_shell_capture,wlroots_emulation
```
For a detailed list of available features, checkout the [Cargo.toml](./Cargo.toml)
</details>



## Development

### Git pre-commit hook

This repository includes a local git hooks directory `.githooks/` with a `pre-commit` script that enforces formatting, lints, and tests before allowing a commit.  It is optional to enable it, but it will prevent you from committing code with failing unit tests or that needs clippy/fmt fixes. To enable the hook locally:

1. Make the hook executable:

```sh
chmod +x .githooks/pre-commit
```

2. Point git to the hooks directory (one-time per clone):

```sh
git config core.hooksPath .githooks
```

The `pre-commit` script runs `cargo fmt --all` (and fails if files were modified), `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

### Dependencies & Compiling from Source
<details>
    <summary>MacOS</summary>

```sh
# Install dependencies
brew install libadwaita pkg-config imagemagick
cargo install cargo-bundle
# Create the macOS icon file
scripts/makeicns.sh
# Create the .app bundle
cargo bundle
# Copy all dynamic libraries into the bundle, and update the bundle to find them there
scripts/copy-macos-dylib.sh
```
</details>

<details>
    <summary>Ubuntu and derivatives</summary>

```sh
sudo apt install libadwaita-1-dev libgtk-4-dev libx11-dev libxtst-dev
```
</details>

<details>
    <summary>Arch and derivatives</summary>

```sh
sudo pacman -S libadwaita gtk libx11 libxtst
```
</details>

<details>
    <summary>Fedora and derivatives</summary>

```sh
sudo dnf install libadwaita-devel libXtst-devel libX11-devel
```
</details>
<details>
    <summary>Nix</summary>

```sh
nix-shell .
```
</details>
<details>
    <summary>Nix (flake)</summary>

```sh
nix develop
```
</details>

<details>
    <summary>Windows</summary>

- First install [Rust](https://www.rust-lang.org/tools/install).

- Then follow the instructions at [gtk-rs.org](https://gtk-rs.org/gtk4-rs/stable/latest/book/installation_windows.html)

*TLDR:*

Build gtk from source

- The following commands should be run in an **admin power shell** instance:
```sh
# install chocolatey
Set-ExecutionPolicy Bypass -Scope Process -Force; iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))

# install gvsbuild dependencies
choco install python git msys2 visualstudio2022-workload-vctools
```

- The following commands should be run in a **regular power shell** instance:

```sh
# install gvsbuild with python
python -m pip install --user pipx
python -m pipx ensurepath
```

- Relaunch your powershell instance so the changes in the environment are reflected.
```sh
pipx install gvsbuild

# build gtk + libadwaita
gvsbuild build gtk4 libadwaita librsvg adwaita-icon-theme
```

- **Make sure to add the directory** `C:\gtk-build\gtk\x64\release\bin`
[**to the `PATH` environment variable**]((https://learn.microsoft.com/en-us/previous-versions/office/developer/sharepoint-2010/ee537574(v=office.14))). Otherwise the project will fail to build.

To avoid building GTK from source, it is possible to disable
the gtk frontend (see conditional compilation).
</details>

## Usage
<details>
    <summary>Gtk Frontend</summary>

By default the gtk frontend will open when running `lan-mouse`.

To connect a device you want to control, simply click the `Add` button and enter the hostname
of the device.

On the *remote* device, authorize your *local* device for incoming traffic using the `Authorize` button
under the "Incoming Connections" section.
The fingerprint for authorization can be found under the general section of your *local* device.
It is of the form "aa:bb:cc:..."

Authorized devices can be persisted using the configuration file (see [Configuration](#configuration)).

If the device still can not be entered, make sure you have UDP port `4242` (or the one selected) opened up in your firewall.
</details>

<details>
    <summary>Command Line Interface</summary>

The cli interface can be accessed by passing `cli` as a commandline argument.
Use
```sh
lan-mouse cli help
```
 to list the available commands and
```sh
lan-mouse cli <cmd> help
```
for information on how to use a specific command.

</details>

<details>
    <summary>Daemon Mode</summary>

Lan Mouse can be launched in daemon mode to keep it running in the background (e.g. for use in a systemd-service).

To do so, use the `daemon` subcommand:

```sh
lan-mouse daemon
```
</details>

<details>
    <summary>Cross-OS log shipping (<code>remote_log</code>)</summary>

**For debugging only** — when chasing a bug across Linux + Windows +
macOS, it's painful to correlate three local log files. Lan Mouse+
ships with an opt-in integration to the
[`cb-logger`](https://logger.cyberbook.id) service that fans every
`log::*!` call out to a remote bulk-ingest endpoint, so a multi-host
session appears as a single merged timeline. This isn't production
telemetry — it stays off until you set `LOGGER_APIKEY`, and you should
turn it off again once the bug is fixed.

**Enabling.** Export `LOGGER_APIKEY` (the shared API key) before starting
`lan-mouse`. On first run the daemon registers itself, caches the
returned bearer token at
`$XDG_CONFIG_HOME/lan-mouse/remote-log-token.json` (0600), and starts a
background shipper. If the env var is unset, the integration is a silent
no-op — logging behaves exactly like `env_logger`.

```sh
# load the key into the current shell, then start the daemon as usual
set -a; source ./apikey.env; set +a
lan-mouse daemon
# stderr: "remote_log: shipping to https://logger.cyberbook.id as lan-mouse-linux-<hostname>"
```

**Joining multiple hosts into one view.** Each host registers under its
own client name (`lan-mouse-<os>-<hostname>`). To see them all in one
timeline, create a group on one host and have the others join:

```sh
# on the owner host (e.g. Linux)
lan-mouse cli logger create lan-mouse-fleet
# → prints invite_code: f30c0713b2e5a7f571174e69

# on each other host (e.g. macOS + Windows), after they've also registered
lan-mouse cli logger join f30c0713b2e5a7f571174e69

# from any member host:
lan-mouse cli logger tail --limit 50
# combined Linux + macOS + Windows entries, oldest at top, with [os hostname] prefix
```

**Subcommand reference.**

| Command | What it does |
| --- | --- |
| `lan-mouse cli logger status` | Show registered client name, client_id, and joined group (with invite_code if you're the owner). |
| `lan-mouse cli logger create <name>` | Create a new logging group and persist its id + invite_code locally. |
| `lan-mouse cli logger join <invite>` | Join an existing group using its invite code. |
| `lan-mouse cli logger groups` | List groups this client owns or is a member of. |
| `lan-mouse cli logger tail [--limit N] [--level L] [--mine-only]` | Pull recent entries (default 50; max 10000) from the saved group, or from this host alone when no group is joined / when `--mine-only`. |

The CLI subcommands talk straight to the logger HTTP API — they don't
require the local `lan-mouse` daemon to be running, only that the host
has been registered at least once.

**Disabling.** Drop the `remote_log` feature from the build
(`--no-default-features`, or rebuild without it), or simply unset
`LOGGER_APIKEY`. The cached token at
`~/.config/lan-mouse/remote-log-token.json` can be deleted to force a
fresh registration on next start.
</details>

## Systemd Service

In order to start lan-mouse with a graphical session automatically,
the [systemd-service](service/lan-mouse.service) can be used:

Copy the file to `~/.config/systemd/user/` and enable the service:

```sh
cp service/lan-mouse.service ~/.config/systemd/user
systemctl --user daemon-reload
systemctl --user enable --now lan-mouse.service
```
> [!Important]
> Make sure to point `ExecStart=/usr/bin/lan-mouse daemon` to the actual `lan-mouse` binary (in case it is not under `/usr/bin`, e.g. when installed manually.


## Configuration
To automatically load clients on startup, the file `$XDG_CONFIG_HOME/lan-mouse/config.toml` is parsed.
`$XDG_CONFIG_HOME` defaults to `~/.config/`.

To create this file you can copy the following example config:

### Example config
> [!TIP]
> key symbols in the release bind are named according
> to their names in [input-event/src/scancode.rs#L172](input-event/src/scancode.rs#L176).
> This is bound to change

```toml
# example configuration

# configure release bind
release_bind = [ "KeyA", "KeyS", "KeyD", "KeyF" ]

# optional port (defaults to 4242)
port = 4242

# list of authorized tls certificate fingerprints that
# are accepted for incoming traffic
[authorized_fingerprints]
"bc:05:ab:7a:a4:de:88:8c:2f:92:ac:bc:b8:49:b8:24:0d:44:b3:e6:a4:ef:d7:0b:6c:69:6d:77:53:0b:14:80" = "iridium"

# define a client on the right side with host name "iridium"
[[clients]]
# position (left | right | top | bottom)
position = "right"
# hostname
hostname = "iridium"
# activate this client immediately when lan-mouse is started
activate_on_startup = true
# optional list of (known) ip addresses
ips = ["192.168.178.156"]

# define a client on the left side with IP address 192.168.178.189
[[clients]]
position = "left"
# The hostname is optional: When no hostname is specified,
# at least one ip address needs to be specified.
hostname = "thorium"
# ips for ethernet and wifi
ips = ["192.168.178.189", "192.168.178.172"]
# optional port
port = 4242
```

Where `left` can be either `left`, `right`, `top` or `bottom`.

## Roadmap
- [x] Graphical frontend (gtk + libadwaita)
- [x] respect xdg-config-home for config file location.
- [x] IP Address switching
- [x] Liveness tracking Automatically ungrab mouse when client unreachable
- [x] Liveness tracking: Automatically release keys, when server offline
- [x] MacOS KeyCode Translation
- [x] Libei Input Capture
- [x] MacOS Input Capture
- [x] Windows Input Capture
- [x] Encryption
- [ ] X11 Input Capture
- [ ] Latency measurement and visualization
- [ ] Bandwidth usage measurement and visualization
- [ ] Clipboard support


## Detailed OS Support

In order to use a device for sending events, an **input-capture** backend is required, while receiving events requires
a supported **input-emulation** *and* **input-capture** backend.

A suitable backend is chosen automatically based on the active desktop environment / compositor.

The following sections detail the emulation and capture backends provided by lan-mouse and their support in desktop environments / operating systems.

### Input Emulation Support

| Desktop / Backend         | wlroots                  | libei                    | remote-desktop portal    | windows                  |   macos                                | x11                |
|---------------------------|--------------------------|--------------------------|--------------------------|--------------------------|----------------------------------------|--------------------|
| Wayland (wlroots)         | :heavy_check_mark:       |                          |                          |                          |                                        |                    |
| Wayland (KDE)             |                          | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |                    |
| Wayland (Gnome)           |                          | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |                    |
| Windows                   |                          |                          |                          | :heavy_check_mark:       |                                        |                    |
| MacOS                     |                          |                          |                          |                          |   :heavy_check_mark:                   |                    |
| X11                       |                          |                          |                          |                          |                                        | :heavy_check_mark: |

- `wlroots`: This backend makes use of the [wlr-virtual-pointer-unstable-v1](https://wayland.app/protocols/wlr-virtual-pointer-unstable-v1) and [virtual-keyboard-unstable-v1](https://wayland.app/protocols/virtual-keyboard-unstable-v1) protocols and is supported by most wlroots based compositors.
- `libei`: This backend uses [libei](https://gitlab.freedesktop.org/libinput/libei) and is supported by GNOME >= 45 or KDE Plasma >= 6.1.
- `xdp`: This backend uses the [freedesktop remote-desktop-portal](https://flatpak.github.io/xdg-desktop-portal/#gdbus-org.freedesktop.portal.RemoteDesktop) and is supported on GNOME and Plasma.
- `x11`: Backend for X11 sessions.
- `windows`: Backend for Windows.
- `macos`: Backend for MacOS.



### Input Capture Support

| Desktop / Backend         | layer-shell              | libei                    | windows                  |   macos                                | x11 |
|---------------------------|--------------------------|--------------------------|--------------------------|----------------------------------------|-----|
| Wayland (wlroots)         | :heavy_check_mark:       |                          |                          |                                        |     |
| Wayland (KDE)             | :heavy_check_mark:       | :heavy_check_mark:       |                          |                                        |     |
| Wayland (Gnome)           |                          | :heavy_check_mark:       |                          |                                        |     |
| Windows                   |                          |                          | :heavy_check_mark:       |                                        |     |
| MacOS                     |                          |                          |                          |   :heavy_check_mark:                   |     |
| X11                       |                          |                          |                          |                                        | WIP |

- `layer-shell`: This backend creates a single pixel wide window on the edges of Displays to capture the cursor using the [layer-shell protocol](https://wayland.app/protocols/wlr-layer-shell-unstable-v1).
- `libei`: This backend uses [libei](https://gitlab.freedesktop.org/libinput/libei) and is supported by GNOME >= 45 or KDE Plasma >= 6.1.
- `windows`: Backend for input capture on Windows.
- `macos`: Backend for input capture on MacOS.
- `x11`: TODO (not yet supported)