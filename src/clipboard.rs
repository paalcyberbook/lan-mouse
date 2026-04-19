use std::sync::{Arc, Mutex};
use std::time::Duration;

use local_channel::mpsc::{Receiver, Sender, channel};
use sha2::{Digest, Sha256};
use tokio::task::{JoinHandle, spawn_local};

/// Maximum clipboard text size (64 KB). Text above this goes through the
/// file-transfer side channel instead (see [`ClipboardChange::OversizeText`]).
pub const MAX_CLIPBOARD_SIZE: usize = 65536;

/// Sentinel byte to identify clipboard messages in the wire protocol.
pub const CLIPBOARD_MSG_TYPE: u8 = 0xFF;

pub use crate::clipboard_event::ClipboardChange;

/// On Wayland, `arboard`'s `set_text` has been observed to succeed (returns Ok
/// and `get_text` from the same process reads back the value) while *no other*
/// Wayland client can see the change — external `wl-paste` returns
/// "Nothing is copied". That breaks cross-app paste, which is the whole point
/// of clipboard sharing.
///
/// The `wl-copy` / `wl-paste` CLI tools from wl-clipboard use the same
/// `wlr-data-control-unstable-v1` protocol that arboard does but work
/// correctly on Hyprland, sway, and other wlroots-based compositors, so we
/// shell out to them when we detect a Wayland session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    WlClipboard,
    Arboard,
}

fn detect_backend() -> Backend {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which_wl_clipboard() {
        Backend::WlClipboard
    } else {
        Backend::Arboard
    }
}

fn which_wl_clipboard() -> bool {
    std::process::Command::new("wl-copy")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) struct ClipboardMonitor {
    backend: Backend,
    arboard: Option<Arc<Mutex<arboard::Clipboard>>>,
    last_text: Arc<Mutex<String>>,
    _task: JoinHandle<()>,
}

impl ClipboardMonitor {
    /// Creates a clipboard monitor that sends changed text to the returned receiver
    pub(crate) fn new() -> Result<(Self, Receiver<ClipboardChange>), arboard::Error> {
        let backend = detect_backend();
        log::info!("clipboard backend: {backend:?}");

        let (event_tx, event_rx): (Sender<ClipboardChange>, Receiver<ClipboardChange>) = channel();
        let last_text_shared: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let last_text_clone = last_text_shared.clone();
        let last_image_hash: Arc<Mutex<Option<[u8; 32]>>> = Arc::new(Mutex::new(None));

        let arboard = match backend {
            Backend::Arboard => Some(Arc::new(Mutex::new(arboard::Clipboard::new()?))),
            Backend::WlClipboard => None,
        };
        let poll_arboard = arboard.clone();

        let task = spawn_local(async move {
            // Seed text cache with current clipboard so we don't fire a bogus
            // "changed locally" on startup. Images start "unknown" which is
            // correct — the user would want to sync the first copy even if
            // the selection already has an image.
            let initial = match backend {
                Backend::WlClipboard => wl_paste_read_text().await.unwrap_or_default(),
                Backend::Arboard => {
                    if let Some(ref a) = poll_arboard {
                        a.lock()
                            .ok()
                            .and_then(|mut c| c.get_text().ok())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                }
            };
            if let Ok(mut cached) = last_text_clone.lock() {
                *cached = initial;
            }

            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Check text first. If text changed, emit and skip the image
                // check this tick — a copy operation sets exactly one of
                // text/image on most clipboards, so seeing new text means a
                // stale image cache entry is about to be invalidated too.
                if poll_text(backend, &poll_arboard, &last_text_clone, &event_tx).await {
                    continue;
                }
                poll_image(backend, &poll_arboard, &last_image_hash, &event_tx).await;
            }
        });

        Ok((
            Self {
                backend,
                arboard,
                last_text: last_text_shared,
                _task: task,
            },
            event_rx,
        ))
    }

    /// Get the last known clipboard text (cached from the polling task)
    pub(crate) fn get_current_text(&self) -> String {
        self.last_text.lock().expect("lock").clone()
    }

    /// Update the cached `last_text` without touching the system clipboard.
    /// Used by the service when it decides not to sync a clipboard change
    /// (e.g. user picked Ignore on an oversize prompt) — without this the
    /// unchanged local clipboard would look "new" on every poll.
    pub(crate) fn note_text(&self, text: &str) {
        if let Ok(mut cached) = self.last_text.lock() {
            *cached = text.to_string();
        }
    }

    /// Set the local clipboard to the given text (received from remote).
    /// Returns true if the clipboard was actually changed, false if the text
    /// matches what we already have.
    pub(crate) fn set_text(&self, text: &str) -> bool {
        // Dedup: skip if the incoming text matches the cached value.
        // Also updates the cache BEFORE writing the clipboard, so the poll
        // task's `current != cached` check returns false and we don't bounce
        // the text back over the network.
        if let Ok(mut cached) = self.last_text.lock() {
            if *cached == text {
                log::debug!(
                    "clipboard from remote matches local ({} bytes), skipping",
                    text.len()
                );
                return false;
            }
            *cached = text.to_string();
        }
        log::info!("setting local clipboard from remote ({} bytes)", text.len());
        match self.backend {
            Backend::WlClipboard => {
                // Spawn detached so the wl-copy daemon outlives the set call
                // and keeps serving the selection until the next copy.
                let text = text.to_string();
                spawn_local(async move {
                    if let Err(e) = wl_copy_write(&text).await {
                        log::warn!("wl-copy failed: {e}");
                    }
                });
            }
            Backend::Arboard => {
                if let Some(ref a) = self.arboard {
                    if let Ok(mut clip) = a.lock() {
                        if let Err(e) = clip.set_text(text) {
                            log::warn!("failed to set clipboard: {e}");
                        }
                    }
                }
            }
        }
        true
    }
}

async fn poll_text(
    backend: Backend,
    poll_arboard: &Option<Arc<Mutex<arboard::Clipboard>>>,
    last_text: &Arc<Mutex<String>>,
    event_tx: &Sender<ClipboardChange>,
) -> bool {
    let current = match backend {
        Backend::WlClipboard => wl_paste_read_text().await.unwrap_or_default(),
        Backend::Arboard => {
            if let Some(a) = poll_arboard {
                a.lock()
                    .ok()
                    .and_then(|mut c| c.get_text().ok())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
    };

    let changed = match last_text.lock() {
        Ok(cached) => current != *cached,
        Err(_) => return false,
    };
    if !changed || current.is_empty() {
        return false;
    }

    // Cache the full current value so the next tick doesn't re-fire, and
    // so `note_text`/`set_text` can compare against the latest.
    if let Ok(mut cached) = last_text.lock() {
        cached.clone_from(&current);
    }

    let event = if current.len() > MAX_CLIPBOARD_SIZE {
        log::info!(
            "clipboard changed locally: oversize text ({} bytes)",
            current.len()
        );
        ClipboardChange::OversizeText { content: current }
    } else {
        log::info!("clipboard changed locally ({} bytes)", current.len());
        ClipboardChange::Text(current)
    };
    event_tx.send(event).expect("channel closed");
    true
}

async fn poll_image(
    backend: Backend,
    poll_arboard: &Option<Arc<Mutex<arboard::Clipboard>>>,
    last_hash: &Arc<Mutex<Option<[u8; 32]>>>,
    event_tx: &Sender<ClipboardChange>,
) {
    let png_opt = match backend {
        Backend::WlClipboard => wl_paste_read_png().await,
        Backend::Arboard => poll_arboard
            .as_ref()
            .and_then(|a| a.lock().ok().and_then(|mut c| c.get_image().ok()))
            .and_then(|img| rgba_to_png(&img.bytes, img.width as u32, img.height as u32)),
    };
    let Some(png) = png_opt else {
        return;
    };

    let hash: [u8; 32] = Sha256::digest(&png).into();
    let changed = match last_hash.lock() {
        Ok(h) => *h != Some(hash),
        Err(_) => return,
    };
    if !changed {
        return;
    }
    if let Ok(mut h) = last_hash.lock() {
        *h = Some(hash);
    }

    let (width, height) = png_dimensions(&png).unwrap_or((0, 0));
    log::info!(
        "clipboard changed locally: image {}x{} ({} bytes PNG)",
        width,
        height,
        png.len()
    );
    event_tx
        .send(ClipboardChange::ImagePng { png, width, height })
        .expect("channel closed");
}

/// Extract `(width, height)` from the IHDR chunk of a PNG, which lives at a
/// fixed offset right after the 8-byte signature and IHDR length/type.
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Some((w, h))
}

/// Encode an RGBA buffer to PNG in memory. Returns `None` on encode error.
fn rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// Read the Wayland clipboard via `wl-paste -n`. Returns empty string if
/// the clipboard is empty or not text; errors (propagated to a `None`) are
/// treated as "no text available".
async fn wl_paste_read_text() -> Option<String> {
    let out = tokio::process::Command::new("wl-paste")
        .arg("-n")
        .arg("-t")
        .arg("text")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    // wl-paste exits non-zero when the clipboard is empty or has no text
    // target; treat that as "no current clipboard" rather than an error.
    if !out.status.success() {
        return Some(String::new());
    }
    String::from_utf8(out.stdout).ok()
}

/// If the Wayland clipboard currently holds a PNG image, read it into a
/// `Vec<u8>`. Returns `None` on any error or if no image is offered — callers
/// treat that as "no image change", not as an error.
async fn wl_paste_read_png() -> Option<Vec<u8>> {
    // Probe the available mime types first; wl-paste errors out if we ask
    // for a type that isn't on offer.
    let list = tokio::process::Command::new("wl-paste")
        .arg("-l")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !list.status.success() {
        return None;
    }
    let types = String::from_utf8_lossy(&list.stdout);
    if !types.lines().any(|t| t.trim() == "image/png") {
        return None;
    }
    let out = tokio::process::Command::new("wl-paste")
        .arg("-t")
        .arg("image/png")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

/// Write `text` to the Wayland clipboard via `wl-copy`. `wl-copy` forks a
/// daemon that keeps serving the selection until replaced, so this call
/// returns quickly and the clipboard stays populated for later pastes.
async fn wl_copy_write(text: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("wl-copy")
        .arg("-t")
        .arg("text/plain;charset=utf-8")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    child.wait().await?;
    Ok(())
}

/// Encode a clipboard text message: [0xFF][length:u32 BE][text bytes]
pub fn encode_clipboard_msg(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let len = bytes.len() as u32;
    let mut buf = Vec::with_capacity(1 + 4 + bytes.len());
    buf.push(CLIPBOARD_MSG_TYPE);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(bytes);
    buf
}

/// Try to decode a clipboard message from a buffer.
pub fn decode_clipboard_msg(buf: &[u8]) -> Option<String> {
    if buf.is_empty() || buf[0] != CLIPBOARD_MSG_TYPE {
        return None;
    }
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    String::from_utf8(buf[5..5 + len].to_vec()).ok()
}
