//! Clipboard change events.
//!
//! Lives in its own always-compiled module so that `service.rs` can type a
//! `Receiver<ClipboardChange>` regardless of whether the `clipboard` cargo
//! feature is enabled.

/// Events the clipboard monitor emits on every detected change. The service
/// decides what to do with each: sync small text inline, prompt the user
/// for anything that needs to go through the file-transfer channel.
#[derive(Debug, Clone)]
pub enum ClipboardChange {
    /// UTF-8 text that fits in the inline sync size cap.
    Text(String),
    /// UTF-8 text larger than the inline cap. Service prompts the user to
    /// send as a file or truncate.
    OversizeText { content: String },
    /// Raw PNG bytes of a clipboard image. Width/height decoded from the
    /// PNG header for UI display. Service prompts the user to send as a
    /// file.
    ImagePng {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
}
