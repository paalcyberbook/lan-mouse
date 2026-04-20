//! Windows edge drop-target backend.
//!
//! Creates one thin layered + topmost + no-focus drop-target window per
//! active edge and registers an `IDropTarget` COM implementation on each via
//! `RegisterDragDrop`. When Explorer (or any OLE DnD source) drags a file
//! onto one of these strip windows and releases, `IDropTarget::Drop` pulls
//! the `CF_HDROP` list from the `IDataObject`, extracts absolute paths via
//! `DragQueryFileW`, and forwards them through a tokio mpsc channel as a
//! [`FileDropEvent::Dropped`].
//!
//! Runs on a dedicated STA thread so the existing `WH_MOUSE_LL` hook on the
//! sibling capture thread is unaffected. Cross-thread communication with
//! the async service is via:
//! * `PostThreadMessageW(WM_USER, …)` to kick the drop-target thread when
//!   the set of active edges changes (same pattern as `event_thread`).
//! * `tokio::sync::mpsc::UnboundedSender<FileDropEvent>` for outbound
//!   events.
//!
//! Scope for v1:
//! * One window per edge; the compositor picks the display.
//! * Only `CF_HDROP` is consumed; HTML-dragged strings and uri-list on
//!   Windows are ignored (they're the uncommon path — file managers ship
//!   CF_HDROP).
//! * No visual feedback on DragEnter; the strip is 1% alpha so it's
//!   effectively invisible.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    thread,
};

use futures_core::Stream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINTL, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget, IDropTarget_Impl,
    OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, LWA_ALPHA, MSG,
    MoveWindow, PostQuitMessage, PostThreadMessageW, RegisterClassExW, SW_SHOW,
    SetLayeredWindowAttributes, ShowWindow, TranslateMessage, WM_USER, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, implement, w};

use crate::{
    Position,
    file_drop::{FileDropEvent, FileDropSource},
};

/// Factory called by `file_drop::file_drop_source`. Returns `None` if the
/// drop-target thread failed to spin up (unlikely — no Windows feature
/// dependency beyond what the capture backend already needs).
pub(crate) fn try_start() -> Option<Box<dyn FileDropSource>> {
    match WindowsDropTarget::new() {
        Ok(src) => Some(Box::new(src)),
        Err(e) => {
            log::warn!("windows drop-target backend unavailable: {e}");
            None
        }
    }
}

pub(crate) struct WindowsDropTarget {
    event_rx: UnboundedReceiver<FileDropEvent>,
    shared: Arc<SharedState>,
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

struct SharedState {
    /// Set of active edges the drop-target thread should be serving. The
    /// thread re-reads this on each `WM_USER` signal from us.
    active_edges: Mutex<HashSet<Position>>,
}

#[derive(Debug, thiserror::Error)]
enum WinDndError {
    #[error("thread spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("thread id handshake failed")]
    HandshakeFailed,
}

/// Custom messages posted to the drop-target thread. Carried in `wparam` of
/// `PostThreadMessageW(_, WM_USER, ...)`.
#[repr(usize)]
#[derive(Debug, Clone, Copy)]
enum ThreadSignal {
    ReconcileEdges = 1,
    Exit = 2,
}

impl WindowsDropTarget {
    fn new() -> Result<Self, WinDndError> {
        let shared = Arc::new(SharedState {
            active_edges: Mutex::new(HashSet::new()),
        });
        let (event_tx, event_rx) = mpsc::unbounded_channel::<FileDropEvent>();
        let (id_tx, id_rx) = std::sync::mpsc::channel::<u32>();
        let shared_for_thread = shared.clone();

        let thread = thread::Builder::new()
            .name("lan-mouse-dropfiles".into())
            .spawn(move || {
                run_thread(shared_for_thread, event_tx, id_tx);
            })?;

        let thread_id = id_rx.recv().map_err(|_| WinDndError::HandshakeFailed)?;

        Ok(Self {
            event_rx,
            shared,
            thread_id,
            thread: Some(thread),
        })
    }

    fn post_signal(&self, sig: ThreadSignal) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_USER, WPARAM(sig as usize), LPARAM(0));
        }
    }
}

impl Drop for WindowsDropTarget {
    fn drop(&mut self) {
        self.post_signal(ThreadSignal::Exit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Stream for WindowsDropTarget {
    type Item = FileDropEvent;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        this.event_rx.poll_recv(cx)
    }
}

impl FileDropSource for WindowsDropTarget {
    fn set_active_edges(&mut self, edges: HashSet<Position>) {
        {
            let mut guard = self.shared.active_edges.lock().expect("lock");
            if *guard == edges {
                return;
            }
            log::info!(
                "windows drop-target: active edges = {edges:?} (was {:?})",
                *guard
            );
            *guard = edges;
        }
        self.post_signal(ThreadSignal::ReconcileEdges);
    }

    fn terminate(&mut self) {
        self.post_signal(ThreadSignal::Exit);
    }
}

// ---------- Drop-target thread ------------------------------------------

/// Per-edge hidden strip window + its registered IDropTarget.
struct EdgeWindow {
    hwnd: HWND,
    _target: IDropTarget,
}

fn run_thread(
    shared: Arc<SharedState>,
    event_tx: UnboundedSender<FileDropEvent>,
    id_tx: std::sync::mpsc::Sender<u32>,
) {
    // Hand our thread id back to the async side so it can post signals.
    let id = unsafe { GetCurrentThreadId() };
    let _ = id_tx.send(id);

    unsafe {
        // STA for OLE DnD.
        let _ = OleInitialize(None);
    }

    let class_name = register_window_class();
    let mut windows: HashMap<Position, EdgeWindow> = HashMap::new();

    // Seed with whatever edges were already set before the thread came up.
    reconcile(&shared, &mut windows, &event_tx, &class_name);

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.0.is_null() && msg.message == WM_USER {
                match msg.wParam.0 {
                    x if x == ThreadSignal::ReconcileEdges as usize => {
                        reconcile(&shared, &mut windows, &event_tx, &class_name);
                    }
                    x if x == ThreadSignal::Exit as usize => {
                        PostQuitMessage(0);
                    }
                    _ => {}
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Teardown: revoke drop-target registration and destroy windows.
    for (_, w) in windows.drain() {
        unsafe {
            let _ = RevokeDragDrop(w.hwnd);
            let _ = DestroyWindow(w.hwnd);
        }
    }

    unsafe {
        OleUninitialize();
    }
}

fn reconcile(
    shared: &SharedState,
    windows: &mut HashMap<Position, EdgeWindow>,
    event_tx: &UnboundedSender<FileDropEvent>,
    class_name: &[u16],
) {
    let wanted = shared.active_edges.lock().expect("lock").clone();

    // Remove windows for edges that are no longer active.
    let stale: Vec<Position> = windows
        .keys()
        .filter(|p| !wanted.contains(p))
        .cloned()
        .collect();
    for pos in stale {
        if let Some(w) = windows.remove(&pos) {
            unsafe {
                let _ = RevokeDragDrop(w.hwnd);
                let _ = DestroyWindow(w.hwnd);
            }
        }
    }

    // Create windows for edges that just became active.
    for pos in wanted {
        if windows.contains_key(&pos) {
            continue;
        }
        match create_edge_window(pos, class_name, event_tx.clone()) {
            Ok(w) => {
                windows.insert(pos, w);
            }
            Err(e) => log::warn!("create_edge_window {pos:?} failed: {e:?}"),
        }
    }
}

const CLASS_NAME: &[u16; 21] = &[
    'l' as u16, 'a' as u16, 'n' as u16, '-' as u16, 'm' as u16, 'o' as u16, 'u' as u16, 's' as u16,
    'e' as u16, '-' as u16, 'd' as u16, 'r' as u16, 'o' as u16, 'p' as u16, 't' as u16, 'a' as u16,
    'r' as u16, 'g' as u16, 'e' as u16, 't' as u16, 0,
];

fn register_window_class() -> Vec<u16> {
    // Idempotent — RegisterClassExW is a no-op if the class already exists.
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            hbrBackground: HBRUSH(CreateSolidBrush(windows::Win32::Foundation::COLORREF(0)).0),
            lpszClassName: PCWSTR(CLASS_NAME.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    }
    CLASS_NAME.to_vec()
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn create_edge_window(
    pos: Position,
    class_name: &[u16],
    event_tx: UnboundedSender<FileDropEvent>,
) -> windows::core::Result<EdgeWindow> {
    unsafe {
        // Virtual-screen bounds via GetSystemMetrics SM_*VIRTUALSCREEN would
        // be the cleaner choice. v1 uses simple primary-screen dimensions —
        // multi-monitor refinement is a follow-up.
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let strip = 2;
        let (x, y, w, h) = match pos {
            Position::Left => (0, 0, strip, screen_h),
            Position::Right => (screen_w - strip, 0, strip, screen_h),
            Position::Top => (0, 0, screen_w, strip),
            Position::Bottom => (0, screen_h - strip, screen_w, strip),
        };

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            w!("LanMouseDropTarget"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        // 1% alpha so the strip is still hit-testable for DnD but
        // visually imperceptible. A fully-transparent WS_EX_TRANSPARENT
        // window does NOT receive drag events.
        SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 2, LWA_ALPHA)?;
        // Ensure the compositor puts it on top.
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = MoveWindow(hwnd, x, y, w, h, true);

        let target: IDropTarget = DropTargetImpl {
            edge: pos,
            events: event_tx,
        }
        .into();
        RegisterDragDrop(hwnd, &target)?;

        Ok(EdgeWindow {
            hwnd,
            _target: target,
        })
    }
}

// ---------- IDropTarget COM implementation ------------------------------

#[implement(IDropTarget)]
struct DropTargetImpl {
    edge: Position,
    events: UnboundedSender<FileDropEvent>,
}

impl IDropTarget_Impl for DropTargetImpl_Impl {
    fn DragEnter(
        &self,
        data: windows::core::Ref<'_, IDataObject>,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let is_file = unsafe { data.as_ref().map(|d| has_file_format(d)).unwrap_or(false) };
        unsafe {
            *effect = if is_file {
                DROPEFFECT_COPY
            } else {
                DROPEFFECT_NONE
            };
        }
        if is_file {
            let _ = self.events.send(FileDropEvent::Entered(self.edge));
        }
        Ok(())
    }

    fn DragOver(
        &self,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        unsafe {
            *effect = DROPEFFECT_COPY;
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        let _ = self.events.send(FileDropEvent::Cancelled(self.edge));
        Ok(())
    }

    fn Drop(
        &self,
        data: windows::core::Ref<'_, IDataObject>,
        _key_state: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        _pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = unsafe {
            data.as_ref()
                .map(|d| extract_cf_hdrop(d))
                .unwrap_or_default()
        };
        unsafe {
            *effect = if paths.is_empty() {
                DROPEFFECT_NONE
            } else {
                DROPEFFECT_COPY
            };
        }
        if !paths.is_empty() {
            let _ = self.events.send(FileDropEvent::Dropped {
                position: self.edge,
                paths,
            });
        }
        Ok(())
    }
}

fn cf_hdrop_formatetc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0 as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Fast QueryGetData check — does the IDataObject advertise CF_HDROP?
unsafe fn has_file_format(data: &IDataObject) -> bool {
    let fe = cf_hdrop_formatetc();
    unsafe { data.QueryGetData(&fe).is_ok() }
}

/// Pull the list of dropped absolute paths from a CF_HDROP IDataObject.
unsafe fn extract_cf_hdrop(data: &IDataObject) -> Vec<std::path::PathBuf> {
    let fe = cf_hdrop_formatetc();
    let mut stg = match unsafe { data.GetData(&fe) } {
        Ok(s) => s,
        Err(e) => {
            log::warn!("IDataObject::GetData(CF_HDROP) failed: {e}");
            return Vec::new();
        }
    };
    // The STGMEDIUM union holds an HGLOBAL for TYMED_HGLOBAL; lock it, cast
    // the first byte to HDROP, and walk the list via DragQueryFileW.
    let mut out = Vec::new();
    unsafe {
        let hglobal = stg.u.hGlobal;
        let locked = GlobalLock(hglobal);
        if !locked.is_null() {
            let hdrop = HDROP(locked as _);
            let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
            for i in 0..count {
                // First call gets the needed length in characters (without NUL).
                let needed = DragQueryFileW(hdrop, i, None);
                if needed == 0 {
                    continue;
                }
                let mut buf: Vec<u16> = vec![0u16; (needed + 1) as usize];
                let copied = DragQueryFileW(hdrop, i, Some(buf.as_mut_slice()));
                if copied == 0 {
                    continue;
                }
                buf.truncate(copied as usize);
                let os = widestring_to_osstring(&buf);
                out.push(std::path::PathBuf::from(os));
            }
            let _ = GlobalUnlock(hglobal);
        }
        ReleaseStgMedium(&mut stg);
    }
    out
}

fn widestring_to_osstring(wide: &[u16]) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(wide)
}

// ---------- Silence unused warnings in non-Windows builds ---------------
//
// The module itself is #[cfg(windows)]-gated in the parent module, so
// nothing below this line ever compiles on non-Windows platforms.

#[allow(dead_code)]
const _: () = {
    // Keeps VecDeque referenced so the cfg-gated module doesn't drop
    // imports we may want when extending teardown semantics later.
    let _ = std::mem::size_of::<VecDeque<FileDropEvent>>();
};
