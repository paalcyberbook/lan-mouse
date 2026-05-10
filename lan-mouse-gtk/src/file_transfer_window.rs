//! Receiver-side GTK UX for the edge drop-zone feature.
//!
//! Flow:
//! 1. [`present_file_offer`] pops an `adw::MessageDialog` with Accept/Decline
//!    when `FileOfferIncoming` arrives.
//! 2. Accept opens a `gtk::FileChooserDialog` in select-folder mode whose
//!    initial directory is the user's last-chosen folder (persisted at
//!    `$XDG_STATE_HOME/lan-mouse/state.json`), defaulting to `~/Downloads`
//!    on first use.
//! 3. Picking a folder sends `RespondFileOffer { Accept { dest_dir } }`
//!    and stashes that folder back to the state file.
//! 4. Decline / dialog-close sends `RespondFileOffer { Decline }`.
//!
//! Progress and terminal state are rendered as `adw::Toast`s on the main
//! window. Per the plan, a richer in-window progress panel can replace the
//! toasts later; the receiver-only semantics (no sender-side UI) already
//! hold.

use std::{
    fs,
    path::{Path, PathBuf},
};

use adw::prelude::*;
use adw::subclass::prelude::ObjectSubclassIsExt;
use gtk::glib;
use gtk::glib::clone;
use serde::{Deserialize, Serialize};

use lan_mouse_ipc::{FileDecision, FrontendRequest};

use crate::window::Window;

fn state_file_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("lan-mouse").join("state.json"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UiState {
    #[serde(default)]
    last_download_dir: Option<PathBuf>,
}

fn load_state() -> UiState {
    let Some(path) = state_file_path() else {
        return UiState::default();
    };
    let Ok(bytes) = fs::read(&path) else {
        return UiState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_state(state: &UiState) {
    let Some(path) = state_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = fs::write(&path, bytes);
    }
}

fn default_download_dir() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(xdg) = std::env::var("XDG_DOWNLOAD_DIR") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg);
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join("Downloads");
        }
    }
    #[cfg(windows)]
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile).join("Downloads");
    }
    PathBuf::from(".")
}

fn pretty_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Pop the initial Accept/Decline dialog for an incoming file offer.
pub fn present_file_offer(
    window: &Window,
    xfer_id: u64,
    root_name: String,
    entries: u32,
    total_bytes: u64,
    fingerprint: String,
) {
    let title = format!("Incoming files: {root_name}");
    let size_str = pretty_bytes(total_bytes);
    let body = if entries > 1 {
        format!("{entries} entries, {size_str}\nFingerprint: {fingerprint}")
    } else {
        format!("{size_str}\nFingerprint: {fingerprint}")
    };

    let dialog = adw::MessageDialog::new(Some(window), Some(&title), Some(&body));
    dialog.add_response("decline", "Decline");
    dialog.add_response("accept", "Save…");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("decline");

    let root_name_for_cb = root_name;
    dialog.connect_response(
        None,
        clone!(
            #[weak]
            window,
            move |_d, resp| match resp {
                "accept" => open_folder_picker(&window, xfer_id, root_name_for_cb.clone()),
                _ => window.send_file_offer_decision(xfer_id, FileDecision::Decline),
            }
        ),
    );
    dialog.present();
}

fn open_folder_picker(window: &Window, xfer_id: u64, root_name: String) {
    let initial = load_state()
        .last_download_dir
        .unwrap_or_else(default_download_dir);

    let picker = gtk::FileChooserDialog::new(
        Some(&format!("Save \"{root_name}\" to…")),
        Some(window),
        gtk::FileChooserAction::SelectFolder,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Save here", gtk::ResponseType::Accept),
        ],
    );
    picker.set_modal(true);
    if initial.exists() {
        let _ = picker.set_current_folder(Some(&gtk::gio::File::for_path(&initial)));
    }

    picker.connect_response(clone!(
        #[weak]
        window,
        move |dialog, response| {
            match response {
                gtk::ResponseType::Accept => {
                    let chosen = dialog
                        .current_folder()
                        .and_then(|f| f.path())
                        .unwrap_or_else(default_download_dir);
                    persist_last_dir(&chosen);
                    window.send_file_offer_decision(
                        xfer_id,
                        FileDecision::Accept { dest_dir: chosen },
                    );
                }
                _ => window.send_file_offer_decision(xfer_id, FileDecision::Decline),
            }
            dialog.close();
        }
    ));
    picker.present();
}

fn persist_last_dir(dir: &Path) {
    let mut state = load_state();
    state.last_download_dir = Some(dir.to_path_buf());
    save_state(&state);
}

impl Window {
    /// Forward the user's Accept/Decline decision to the daemon.
    pub(crate) fn send_file_offer_decision(&self, xfer_id: u64, decision: FileDecision) {
        if matches!(decision, FileDecision::Decline) {
            self.show_toast(&format!("Declined transfer {xfer_id}"));
        }
        self.crate_request(FrontendRequest::RespondFileOffer { xfer_id, decision });
    }

    /// Coarse-grained progress indication: one toast at "transfer started"
    /// (the first Progress tick), nothing in between. A richer progress
    /// panel lives in a follow-up — this keeps v1 scope tight and avoids
    /// flooding the toast overlay at 10 ticks/sec.
    pub(crate) fn show_file_transfer_progress_toast(&self, xfer_id: u64, total: u64) {
        if !self.imp().mark_xfer_started(xfer_id) {
            return;
        }
        self.show_toast(&format!("Receiving {}…", pretty_bytes(total)));
    }

    pub(crate) fn show_file_transfer_finished_toast(
        &self,
        xfer_id: u64,
        result: lan_mouse_ipc::TransferResult,
    ) {
        use lan_mouse_ipc::TransferResult as R;
        self.imp().forget_xfer(xfer_id);
        match result {
            R::Ok { destination } => self.show_toast(&format!(
                "Transfer complete: saved to {}",
                destination.display()
            )),
            R::Cancelled => self.show_toast("Transfer cancelled"),
            R::Error(e) => self.show_toast(&format!("Transfer failed: {e}")),
        }
    }
}
