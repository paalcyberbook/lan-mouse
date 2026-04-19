use std::sync::{Arc, Mutex};
use std::time::Duration;

use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};

/// Maximum clipboard text size (64 KB)
pub const MAX_CLIPBOARD_SIZE: usize = 65536;

/// Sentinel byte to identify clipboard messages in the protocol
pub const CLIPBOARD_MSG_TYPE: u8 = 0xFF;

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
    pub(crate) fn new() -> Result<(Self, Receiver<String>), arboard::Error> {
        let backend = detect_backend();
        log::info!("clipboard backend: {backend:?}");

        let (event_tx, event_rx): (Sender<String>, Receiver<String>) = channel();
        let last_text_shared: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let last_text_clone = last_text_shared.clone();

        let arboard = match backend {
            Backend::Arboard => Some(Arc::new(Mutex::new(arboard::Clipboard::new()?))),
            Backend::WlClipboard => None,
        };
        let poll_arboard = arboard.clone();

        let task = spawn_local(async move {
            // Seed cache with current clipboard contents so we don't fire a
            // bogus "clipboard changed locally" on startup.
            let initial = match backend {
                Backend::WlClipboard => wl_paste_read().await.unwrap_or_default(),
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

                let current = match backend {
                    Backend::WlClipboard => wl_paste_read().await.unwrap_or_default(),
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

                // Compare against the shared cache so set_text() (remote-driven
                // updates) doesn't look like a local change and bounce back.
                let changed = match last_text_clone.lock() {
                    Ok(cached) => current != *cached,
                    Err(_) => continue,
                };

                if changed && !current.is_empty() {
                    log::info!("clipboard changed locally ({} bytes)", current.len());
                    let text = if current.len() > MAX_CLIPBOARD_SIZE {
                        current[..MAX_CLIPBOARD_SIZE].to_string()
                    } else {
                        current.clone()
                    };
                    if let Ok(mut cached) = last_text_clone.lock() {
                        cached.clone_from(&text);
                    }
                    event_tx.send(text).expect("channel closed");
                }
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

/// Read the Wayland clipboard via `wl-paste -n`. Returns empty string if
/// the clipboard is empty or not text; errors (propagated to a `None`) are
/// treated as "no text available".
async fn wl_paste_read() -> Option<String> {
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
