//! Wayland `wl_data_device` + `zwlr_layer_shell_v1` drag-detection backend.
//!
//! This is the real implementation slot for [`FileDropBackend`] on Wayland
//! wlroots-based compositors (sway, Hyprland, river). It creates 1px-wide
//! layer-shell "drop target" surfaces along the configured screen edges and
//! listens on the seat's `wl_data_device` for drag-enter + drop events on
//! those surfaces. When a drop lands with a `text/uri-list` offer, the URIs
//! are parsed into `PathBuf`s and emitted as [`FileDropEvent::Dropped`].
//!
//! Scope for v1:
//! * One surface per active edge; compositor picks the output.
//! * Only `text/uri-list` is accepted — covers every mainstream Linux file
//!   manager and GTK/Qt drag sources.
//! * Remote gvfs mounts (`smb://`, `sftp://`, etc.) are filtered out and
//!   logged; they have no local path to ship.
//!
//! The DnD wayland connection is independent of the existing `layer_shell`
//! pointer-capture connection. Running both concurrently is fine — both
//! clients are regular wayland consumers and the compositor doesn't care.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use tokio::io::unix::AsyncFd;

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_offer::{self, WlDataOffer},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::{
    Position,
    file_drop::{FileDropEvent, FileDropSource},
};

const URI_LIST_MIME: &str = "text/uri-list";

/// Public constructor — called by [`crate::file_drop::file_drop_source`].
/// Returns `None` if we can't connect to a Wayland display or the compositor
/// doesn't advertise the globals we need (e.g. layer-shell-less GNOME).
pub(crate) fn try_start() -> Option<Box<dyn FileDropSource>> {
    match LayerShellDnd::new() {
        Ok(src) => Some(Box::new(src)),
        Err(e) => {
            log::info!("layer-shell DnD unavailable: {e}");
            None
        }
    }
}

pub(crate) struct LayerShellDnd {
    state: State,
    event_queue: EventQueue<State>,
    qh: QueueHandle<State>,
    async_fd: AsyncFd<RawFdWrap>,
}

struct State {
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    _seat: WlSeat,
    _data_device: WlDataDevice,
    active_edges: HashSet<Position>,
    surfaces: HashMap<Position, EdgeSurface>,
    /// Map from our wl_surface object id → edge. Needed because
    /// `wl_data_device::enter` hands us the surface but not which edge it
    /// represents.
    surface_to_edge: HashMap<WlSurface, Position>,
    /// Tracks the offer currently hovering our drop surface.
    hover: Option<HoverState>,
    /// Pending events to hand out via the `FileDropSource` stream.
    emit_queue: VecDeque<FileDropEvent>,
    /// Offered mime types per wl_data_offer, collected between `data_offer`
    /// and its consuming `enter`.
    pending_offers: HashMap<WlDataOffer, Vec<String>>,
    /// Reusable 1×1 transparent buffer for all edge surfaces.
    buffer: Option<WlBuffer>,
    #[allow(dead_code)]
    shm_pool: Option<WlShmPool>,
}

struct EdgeSurface {
    surface: WlSurface,
    _layer_surface: ZwlrLayerSurfaceV1,
    /// Set true once the compositor has configured the surface and we've
    /// committed the buffer. Not strictly needed by the DnD flow, but keeps
    /// us from racing to use the surface before the compositor accepts it.
    configured: bool,
}

struct HoverState {
    offer: WlDataOffer,
    edge: Position,
}

/// Wrap a raw Wayland fd for AsyncFd. Wayland's `Connection::prepare_read`
/// gives us the fd; AsyncFd wraps it so tokio can wake us on readability.
struct RawFdWrap(std::os::fd::RawFd);

impl std::os::fd::AsRawFd for RawFdWrap {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0
    }
}

#[derive(Debug, thiserror::Error)]
enum DndInitError {
    #[error("WAYLAND_DISPLAY not set")]
    NoWaylandDisplay,
    #[error("wayland connect: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("registry init: {0}")]
    Registry(#[from] wayland_client::globals::GlobalError),
    #[error("bind: {0}")]
    Bind(#[from] wayland_client::globals::BindError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl LayerShellDnd {
    fn new() -> Result<Self, DndInitError> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err(DndInitError::NoWaylandDisplay);
        }
        let conn = Connection::connect_to_env()?;
        let (globals, event_queue) = registry_queue_init::<State>(&conn)?;
        let qh = event_queue.handle();

        let compositor: WlCompositor = globals.bind(&qh, 1..=6, ())?;
        let shm: WlShm = globals.bind(&qh, 1..=1, ())?;
        let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
        let seat: WlSeat = globals.bind(&qh, 1..=9, ())?;
        let ddm: WlDataDeviceManager = globals.bind(&qh, 1..=3, ())?;
        let data_device = ddm.get_data_device(&seat, &qh, ());

        let fd = conn.backend().poll_fd().as_raw_fd();
        let async_fd = AsyncFd::with_interest(RawFdWrap(fd), tokio::io::Interest::READABLE)?;

        let state = State {
            compositor,
            shm,
            layer_shell,
            _seat: seat,
            _data_device: data_device,
            active_edges: HashSet::new(),
            surfaces: HashMap::new(),
            surface_to_edge: HashMap::new(),
            hover: None,
            emit_queue: VecDeque::new(),
            pending_offers: HashMap::new(),
            buffer: None,
            shm_pool: None,
        };

        Ok(Self {
            state,
            event_queue,
            qh,
            async_fd,
        })
    }

    /// Drive the event loop until either a file-drop event is ready to emit
    /// or the wayland fd blocks again.
    fn pump(&mut self, cx: &mut Context<'_>) -> Poll<Option<FileDropEvent>> {
        loop {
            if let Some(ev) = self.state.emit_queue.pop_front() {
                return Poll::Ready(Some(ev));
            }
            // Flush any pending requests before sleeping.
            let _ = self.event_queue.flush();

            let prep = match self.event_queue.prepare_read() {
                Some(p) => p,
                None => {
                    // Events are already buffered; dispatch them.
                    if let Err(e) = self.event_queue.dispatch_pending(&mut self.state) {
                        log::warn!("layer_shell_dnd dispatch error: {e}");
                        return Poll::Ready(None);
                    }
                    continue;
                }
            };
            // Is the fd readable right now?
            match self.async_fd.poll_read_ready(cx) {
                Poll::Pending => {
                    drop(prep);
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    log::warn!("layer_shell_dnd fd poll error: {e}");
                    return Poll::Ready(None);
                }
                Poll::Ready(Ok(mut guard)) => match prep.read() {
                    Ok(_n) => {
                        guard.clear_ready();
                    }
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        guard.clear_ready();
                        return Poll::Pending;
                    }
                    Err(e) => {
                        log::warn!("layer_shell_dnd read error: {e}");
                        return Poll::Ready(None);
                    }
                },
            }
        }
    }
}

use std::os::fd::AsRawFd as _;

impl LayerShellDnd {
    /// (Re)create the edge drop-target surfaces to match `active_edges`.
    fn reconcile_surfaces(&mut self) {
        // Drop surfaces for edges no longer active.
        let stale: Vec<Position> = self
            .state
            .surfaces
            .keys()
            .filter(|p| !self.state.active_edges.contains(p))
            .cloned()
            .collect();
        for pos in stale {
            if let Some(edge) = self.state.surfaces.remove(&pos) {
                self.state.surface_to_edge.remove(&edge.surface);
                edge.surface.destroy();
            }
        }
        // Add surfaces for newly active edges.
        let to_add: Vec<Position> = self
            .state
            .active_edges
            .iter()
            .filter(|p| !self.state.surfaces.contains_key(p))
            .cloned()
            .collect();
        for pos in to_add {
            if let Some(edge) = create_edge_surface(
                &self.state.compositor,
                &self.state.layer_shell,
                pos,
                &self.qh,
            ) {
                self.state.surface_to_edge.insert(edge.surface.clone(), pos);
                self.state.surfaces.insert(pos, edge);
            }
        }
        let _ = self.event_queue.flush();
    }
}

fn create_edge_surface(
    compositor: &WlCompositor,
    layer_shell: &ZwlrLayerShellV1,
    pos: Position,
    qh: &QueueHandle<State>,
) -> Option<EdgeSurface> {
    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "lan-mouse-drop".into(),
        qh,
        pos,
    );
    let (anchor, width, height) = match pos {
        Position::Left => (Anchor::Left | Anchor::Top | Anchor::Bottom, 1, 0),
        Position::Right => (Anchor::Right | Anchor::Top | Anchor::Bottom, 1, 0),
        Position::Top => (Anchor::Top | Anchor::Left | Anchor::Right, 0, 1),
        Position::Bottom => (Anchor::Bottom | Anchor::Left | Anchor::Right, 0, 1),
    };
    layer_surface.set_anchor(anchor);
    layer_surface.set_size(width, height);
    layer_surface.set_exclusive_zone(0);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    surface.commit();
    Some(EdgeSurface {
        surface,
        _layer_surface: layer_surface,
        configured: false,
    })
}

/// Parse a `text/uri-list` blob into filesystem paths. Filters out comments
/// and any non-`file://` scheme (e.g. `smb://`, `trash:///`) which would be
/// unresolvable on the remote end.
fn parse_uri_list(raw: &[u8]) -> Vec<PathBuf> {
    let s = std::str::from_utf8(raw).unwrap_or("");
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim_end_matches(['\r', '\n']).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(path) = line.strip_prefix("file://") {
            let decoded = percent_decode(path);
            out.push(PathBuf::from(decoded));
        } else {
            log::info!("skipping non-file URI on drop: {line}");
        }
    }
    out
}

/// Minimal percent-decoder. Only handles `%XX` hex escapes — enough for
/// file-manager-produced `text/uri-list` payloads.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl Stream for LayerShellDnd {
    type Item = FileDropEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        this.pump(cx)
    }
}

impl FileDropSource for LayerShellDnd {
    fn set_active_edges(&mut self, edges: HashSet<Position>) {
        if self.state.active_edges == edges {
            return;
        }
        log::info!(
            "layer-shell DnD: active edges = {:?} (was {:?})",
            edges,
            self.state.active_edges,
        );
        self.state.active_edges = edges;
        self.reconcile_surfaces();
    }

    fn terminate(&mut self) {
        for (_, e) in self.state.surfaces.drain() {
            e.surface.destroy();
        }
        let _ = self.event_queue.flush();
    }
}

// ------------------- Dispatch impls ------------------------------------

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore WlDataDeviceManager);

impl Dispatch<ZwlrLayerSurfaceV1, Position> for State {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        pos: &Position,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                surface.ack_configure(serial);
                // Attach a 1×1 transparent buffer so the compositor accepts
                // our surface as valid for pointer/DnD events.
                if state.buffer.is_none() {
                    if let Some(buf) = state.make_placeholder_buffer(qh) {
                        state.buffer = Some(buf);
                    }
                }
                if let Some(edge) = state.surfaces.get_mut(pos) {
                    if let Some(buf) = &state.buffer {
                        edge.surface.attach(Some(buf), 0, 0);
                        edge.surface.damage_buffer(0, 0, 1, 1);
                        edge.surface.commit();
                        edge.configured = true;
                    }
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(edge) = state.surfaces.remove(pos) {
                    state.surface_to_edge.remove(&edge.surface);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device::Event as E;
        match event {
            E::DataOffer { id } => {
                // A new offer is incoming — start tracking its mime-type list.
                state.pending_offers.insert(id, Vec::new());
            }
            E::Enter { surface, id, .. } => {
                let Some(edge) = state.surface_to_edge.get(&surface).cloned() else {
                    // Drag entered a surface we don't own (shouldn't happen
                    // — we only listen on our own surfaces — but be safe).
                    return;
                };
                let Some(offer) = id else {
                    return;
                };
                let types = state
                    .pending_offers
                    .get(&offer)
                    .cloned()
                    .unwrap_or_default();
                if !types.iter().any(|t| t == URI_LIST_MIME) {
                    // Not a file drag; ignore.
                    return;
                }
                // Accept the mime type so the source knows we'll take it on drop.
                offer.accept(0, Some(URI_LIST_MIME.into()));
                state.hover = Some(HoverState { offer, edge });
                state.emit_queue.push_back(FileDropEvent::Entered(edge));
            }
            E::Leave => {
                if let Some(h) = state.hover.take() {
                    state.emit_queue.push_back(FileDropEvent::Cancelled(h.edge));
                }
            }
            E::Motion { .. } => {}
            E::Drop => {
                let Some(hover) = state.hover.take() else {
                    return;
                };
                // Pipe for receive.
                let (reader, writer) = match pipe2_cloexec() {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("pipe2 failed: {e}");
                        hover.offer.destroy();
                        return;
                    }
                };
                hover.offer.receive(URI_LIST_MIME.into(), writer.as_fd());
                drop(writer);
                // Read the payload synchronously — uri-list blobs are tiny.
                let mut buf = Vec::new();
                let mut f = std::fs::File::from(reader);
                let _ = f.read_to_end(&mut buf);
                let paths = parse_uri_list(&buf);
                hover.offer.finish();
                hover.offer.destroy();
                if paths.is_empty() {
                    log::info!("drop on edge {} produced no file paths", hover.edge);
                    return;
                }
                state.emit_queue.push_back(FileDropEvent::Dropped {
                    position: hover.edge,
                    paths,
                });
            }
            E::Selection { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            state
                .pending_offers
                .entry(offer.clone())
                .or_default()
                .push(mime_type);
        }
    }
}

impl State {
    /// Create a 1×1 fully-transparent ARGB8888 shm buffer that our layer
    /// surfaces attach to — compositors expect a committed buffer before
    /// routing input / DnD to a surface.
    fn make_placeholder_buffer(&mut self, qh: &QueueHandle<State>) -> Option<WlBuffer> {
        use std::os::fd::AsFd;
        let mut tmp = tempfile::tempfile().ok()?;
        use std::io::Write;
        // ARGB8888: one pixel, 4 bytes, fully transparent.
        tmp.write_all(&[0, 0, 0, 0]).ok()?;
        tmp.flush().ok()?;
        let pool = self.shm.create_pool(tmp.as_fd(), 4, qh, ());
        let buf = pool.create_buffer(0, 1, 1, 4, Format::Argb8888, qh, ());
        self.shm_pool = Some(pool);
        Some(buf)
    }
}

fn pipe2_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    // nix / rustix would be cleaner but input-capture avoids both; use the
    // libc direct call. CLOEXEC keeps the fd from leaking to any exec'd
    // child process.
    let mut fds: [libc::c_int; 2] = [0; 2];
    let r = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded so both fds are valid owned fds.
    unsafe { Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
}
