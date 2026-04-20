//! Edge drop-zone source.
//!
//! Separate from the [`crate::Capture`] trait on purpose — drag detection
//! uses a completely different API stack on each OS (Win32 OLE, Wayland
//! `wl_data_device`, macOS NSDragging, X11 XDND) that has no overlap with
//! the pointer/keyboard event streams lan-mouse already captures. Keeping
//! this as its own trait also lets the main event pipeline stay uninterested
//! in file drops on builds where [`FileDropBackend::Dummy`] is the only
//! available option.
//!
//! v1 supports the [`FileDropBackend::Dummy`] backend only — a never-ready
//! stream. Real backends land in tasks 12 (Windows) and 13 (Wayland) of the
//! edge-drop-zone plan.
//!
//! This module is gated behind the workspace `file_drop` feature.

use std::{
    collections::HashSet,
    fmt::Display,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;

use crate::Position;

/// Why each backend is its own trait: see module docs.
#[derive(Debug, Clone)]
pub enum FileDropEvent {
    /// A drag entered the edge zone at this position. Advisory — backends
    /// may skip emitting this and still be correct.
    Entered(Position),
    /// A drag entered the edge zone AND the backend has already pulled the
    /// file list out of the DnD offer. This fires in the "drag-continues"
    /// flow (plan §1): the receiver can start transferring the files
    /// eagerly while the user's cursor carries the drag across to the
    /// remote screen. Sent by both Wayland (data_device::enter) and Windows
    /// (IDropTarget::DragEnter) backends.
    DragStarted {
        position: Position,
        paths: Vec<PathBuf>,
    },
    /// User released the button *on* the edge strip instead of crossing
    /// over to the remote. Degenerate case — service pops the drop-confirm
    /// dialog on the remote with a cursor-agnostic default (Desktop).
    DragEndedEarly {
        position: Position,
        paths: Vec<PathBuf>,
    },
    /// Files dropped at the edge zone. Legacy "drop on strip" path — kept
    /// for backward compat during the rework; will be removed once all
    /// callers consume the DragStarted/DragEndedEarly events.
    Dropped {
        position: Position,
        paths: Vec<PathBuf>,
    },
    /// The drag left without completing. Pairs with [`FileDropEvent::Entered`].
    Cancelled(Position),
}

impl Display for FileDropEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileDropEvent::Entered(p) => write!(f, "drag entered {p}"),
            FileDropEvent::DragStarted { position, paths } => {
                write!(f, "drag started at {position} ({} paths)", paths.len())
            }
            FileDropEvent::DragEndedEarly { position, paths } => {
                write!(f, "drag ended early at {position} ({} paths)", paths.len())
            }
            FileDropEvent::Dropped { position, paths } => {
                write!(f, "drop at {position} ({} paths)", paths.len())
            }
            FileDropEvent::Cancelled(p) => write!(f, "drag cancelled at {p}"),
        }
    }
}

/// Source of file-drop events. Backends are responsible for showing and
/// hiding edge overlay surfaces; the service tells them which edges are
/// active via [`set_active_edges`](FileDropSource::set_active_edges).
pub trait FileDropSource: Stream<Item = FileDropEvent> + Unpin {
    /// Replace the set of edges currently bordering a configured remote
    /// client. Backends use this to show/hide their per-edge drop-target
    /// surfaces. Called from the service whenever the set of clients (or
    /// their positions) changes.
    ///
    /// Intentionally synchronous — real backends dispatch to a separate
    /// thread or queue (wayland object creation, `PostThreadMessageW`)
    /// that takes care of the asynchrony, so the caller doesn't need to
    /// pay an `.await` just to hand off a `HashSet`.
    fn set_active_edges(&mut self, edges: HashSet<Position>);

    /// Clean shutdown.
    fn terminate(&mut self);
}

/// Which drag-detection backend to use.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FileDropBackend {
    /// Never-ready backend. Always available. Produces no events.
    Dummy,
    /// Wayland wlroots: `zwlr_layer_shell_v1` edge surfaces + `wl_data_device`.
    /// Works on sway, Hyprland, and other wlroots-based compositors; KWin
    /// support depends on the compositor forwarding data_device to layer
    /// surfaces (needs verification per the plan's Risk #1).
    #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
    LayerShellDataDevice,
    /// Windows: per-edge `IDropTarget` strip windows on a dedicated STA
    /// thread. Coexists with the `WH_MOUSE_LL` pointer-capture thread.
    #[cfg(windows)]
    WindowsOle,
}

impl Display for FileDropBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileDropBackend::Dummy => write!(f, "dummy"),
            #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
            FileDropBackend::LayerShellDataDevice => write!(f, "layer-shell-data-device"),
            #[cfg(windows)]
            FileDropBackend::WindowsOle => write!(f, "windows-ole"),
        }
    }
}

/// Pick a sensible default backend for the current OS. On wlroots Wayland
/// sessions we attempt the `zwlr_layer_shell_v1` + `wl_data_device` path;
/// on Windows we use the OLE IDropTarget path; elsewhere (no
/// WAYLAND_DISPLAY, non-layer-shell compositor, etc.) we fall back to
/// [`FileDropBackend::Dummy`] which produces no events.
pub fn auto_detect_backend() -> FileDropBackend {
    // Per-OS gating as two non-overlapping cfg branches so neither side
    // produces "unreachable expression" warnings (the Windows path used to
    // `return` unconditionally, making the Unix-side fallback dead code).
    #[cfg(windows)]
    {
        FileDropBackend::WindowsOle
    }
    #[cfg(not(windows))]
    {
        #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return FileDropBackend::LayerShellDataDevice;
        }
        FileDropBackend::Dummy
    }
}

/// Instantiate a [`FileDropSource`] for the chosen backend, or auto-detect
/// one if `None`. Backends that fail to start (e.g. layer-shell on a
/// compositor that doesn't implement it) transparently fall back to Dummy
/// so the service can keep running.
pub fn file_drop_source(backend: Option<FileDropBackend>) -> Box<dyn FileDropSource> {
    let backend = backend.unwrap_or_else(auto_detect_backend);
    match backend {
        FileDropBackend::Dummy => {
            log::info!("file-drop backend: dummy (edge drop-zone not yet implemented on this OS)");
            Box::new(DummyFileDropSource)
        }
        #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
        FileDropBackend::LayerShellDataDevice => {
            if let Some(src) = crate::layer_shell_dnd::try_start() {
                log::info!("file-drop backend: layer-shell + wl_data_device");
                src
            } else {
                log::info!("file-drop backend: dummy (layer_shell_dnd startup failed)");
                Box::new(DummyFileDropSource)
            }
        }
        #[cfg(windows)]
        FileDropBackend::WindowsOle => {
            if let Some(src) = crate::windows::drop_target::try_start() {
                log::info!("file-drop backend: windows IDropTarget");
                src
            } else {
                log::info!("file-drop backend: dummy (windows drop-target startup failed)");
                Box::new(DummyFileDropSource)
            }
        }
    }
}

/// Always-pending stream. Produces no events; used on builds without a real
/// drag-detection backend.
struct DummyFileDropSource;

impl Stream for DummyFileDropSource {
    type Item = FileDropEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl FileDropSource for DummyFileDropSource {
    fn set_active_edges(&mut self, _edges: HashSet<Position>) {}
    fn terminate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn dummy_source_never_emits() {
        let mut src = file_drop_source(Some(FileDropBackend::Dummy));
        src.set_active_edges(HashSet::from([Position::Left]));
        let maybe = tokio::time::timeout(std::time::Duration::from_millis(50), src.next()).await;
        assert!(maybe.is_err(), "dummy source must remain pending");
        src.terminate();
    }
}
