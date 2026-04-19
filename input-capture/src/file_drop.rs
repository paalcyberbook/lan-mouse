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

use async_trait::async_trait;
use futures_core::Stream;

use crate::Position;

/// Why each backend is its own trait: see module docs.
#[derive(Debug, Clone)]
pub enum FileDropEvent {
    /// A drag entered the edge zone at this position. Advisory — backends
    /// may skip emitting this and still be correct.
    Entered(Position),
    /// Files dropped at the edge zone. The actionable event.
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
#[async_trait(?Send)]
pub trait FileDropSource: Stream<Item = FileDropEvent> + Unpin {
    /// Replace the set of edges currently bordering a connected remote
    /// client. Backends use this to show/hide their per-edge drop-target
    /// surfaces. Called from the service main loop whenever the set of
    /// connected clients (or their positions) changes.
    async fn set_active_edges(&mut self, edges: HashSet<Position>);

    /// Clean shutdown.
    async fn terminate(&mut self);
}

/// Which drag-detection backend to use.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FileDropBackend {
    /// Never-ready backend. Always available. Produces no events.
    Dummy,
    // Real backends slot in here in tasks 12 and 13:
    // #[cfg(windows)]
    // WindowsOle,
    // #[cfg(all(unix, feature = "layer_shell", not(target_os = "macos")))]
    // LayerShellDataDevice,
}

impl Display for FileDropBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileDropBackend::Dummy => write!(f, "dummy"),
        }
    }
}

/// Pick a sensible default backend for the current OS. For now only
/// [`FileDropBackend::Dummy`] is implemented; this function exists so that
/// tasks 12/13 can switch defaults per-OS without touching callers.
pub fn auto_detect_backend() -> FileDropBackend {
    FileDropBackend::Dummy
}

/// Instantiate a [`FileDropSource`] for the chosen backend, or auto-detect
/// one if `None`. Infallible for now — the Dummy backend can't fail.
pub fn file_drop_source(backend: Option<FileDropBackend>) -> Box<dyn FileDropSource> {
    let backend = backend.unwrap_or_else(auto_detect_backend);
    match backend {
        FileDropBackend::Dummy => {
            log::info!("file-drop backend: dummy (edge drop-zone not yet implemented on this OS)");
            Box::new(DummyFileDropSource)
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

#[async_trait(?Send)]
impl FileDropSource for DummyFileDropSource {
    async fn set_active_edges(&mut self, _edges: HashSet<Position>) {}
    async fn terminate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn dummy_source_never_emits() {
        let mut src = file_drop_source(Some(FileDropBackend::Dummy));
        src.set_active_edges(HashSet::from([Position::Left])).await;
        let maybe = tokio::time::timeout(std::time::Duration::from_millis(50), src.next()).await;
        assert!(maybe.is_err(), "dummy source must remain pending");
        src.terminate().await;
    }
}
