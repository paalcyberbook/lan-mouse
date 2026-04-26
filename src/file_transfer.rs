//! Edge drop-zone file-transfer transport.
//!
//! A QUIC side channel that runs on a separate UDP port from the main DTLS
//! connection. Each file/folder transfer is a single QUIC connection carrying
//! a bidirectional control stream and one unidirectional stream per filesystem
//! entry.
//!
//! Trust model reuses the DTLS identity and `authorized_fingerprints` map —
//! rustls verifies peer certs purely on SHA-256 fingerprint allowlist, nothing
//! else. No PKI, no SNI check.
//!
//! Wire format:
//!   * Control stream carries framed CBOR [`FileCtrl`] messages.
//!   * Each data stream carries framed CBOR [`FileFrame`] messages (Entry →
//!     zero-or-more Chunk → EntryDone). `Chunk.data` may be zstd-compressed
//!     based on the `Entry.compressed` flag.
//!
//! This module does not drive the UX. Incoming offers are emitted as
//! [`Event::IncomingOffer`] through the command/event channel; the caller
//! (service.rs) forwards them to the GTK frontend and relays the user's
//! decision back via [`Command::RespondOffer`].

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use quinn::{
    Endpoint, RecvStream, SendStream,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

use crate::crypto;

/// Maximum size of a raw (pre-compression) chunk on the wire.
const CHUNK_SIZE: usize = 64 * 1024;

/// How often the receiver emits a progress update (whichever comes first).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const PROGRESS_BYTES: u64 = 1024 * 1024;

/// Hard cap on a single deserialized CBOR frame so a malicious peer cannot
/// make us allocate unbounded memory from a length prefix. Holds the worst
/// case chunk (zstd never bloats by more than a few percent).
const MAX_FRAME_BYTES: usize = CHUNK_SIZE * 2;

/// First 16 bytes of well-known already-compressed container formats. If a
/// file starts with one of these, skip zstd on its chunks to avoid wasting
/// CPU on incompressible data.
const ALREADY_COMPRESSED_MAGICS: &[&[u8]] = &[
    b"\x28\xb5\x2f\xfd",         // zstd
    b"\x1f\x8b",                 // gzip
    b"\xfd\x37\x7a\x58\x5a\x00", // xz
    b"PK\x03\x04",               // zip (also .docx/.xlsx/.apk/...)
    b"\xff\xd8\xff",             // jpeg
    b"\x89PNG\r\n\x1a\n",        // png
    b"RIFF",                     // webp/wav/avi (container)
    b"\x00\x00\x00\x18ftyp",     // mp4
    b"\x00\x00\x00\x20ftyp",     // mp4 variant
    b"\x1aE\xdf\xa3",            // matroska/webm
    b"OggS",                     // ogg
    b"\xFFFB",                   // mp3 frame header
    b"ID3",                      // mp3 w/ id3 tag
];

// ----- Public API --------------------------------------------------------

#[derive(Debug, Error)]
pub enum FileTransferError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("crypto: {0}")]
    Crypto(#[from] crypto::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("quinn config: {0}")]
    QuicConfig(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("quinn connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("quinn connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("quinn write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("quinn read: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("cbor decode: {0}")]
    CborDe(#[from] ciborium::de::Error<std::io::Error>),
    #[error("cbor encode: {0}")]
    CborSer(#[from] ciborium::ser::Error<std::io::Error>),
    #[error("{0}")]
    Protocol(String),
}

/// Commands sent into the file-transfer service from service.rs.
#[derive(Debug)]
pub enum Command {
    /// Initiate an outgoing transfer. `root` may be a single file or a
    /// directory (directories are sent recursively).
    SendPath {
        target_addr: SocketAddr,
        target_fingerprint: String,
        root: PathBuf,
    },
    /// In-memory clipboard hand-off — bytes are wrapped in the same
    /// Offer/Entry/Chunk wire format as a file but the receiver
    /// auto-accepts and pipes them straight into the local clipboard
    /// instead of writing to disk. Used for clipboards that exceed the
    /// inline-DTLS UDP-safe size cap so they don't blow up Windows'
    /// default ~8 KB UDP recv buffer (WSAEMSGSIZE).
    SendClipboard {
        target_addr: SocketAddr,
        target_fingerprint: String,
        content: Vec<u8>,
    },
    /// Response to an [`Event::IncomingOffer`].
    RespondOffer { xfer_id: u64, decision: Decision },
    /// Cancel an in-progress transfer (either direction). Reserved — the
    /// GTK progress panel will wire this up when we add a real progress
    /// widget; kept in the API so wire-protocol versions don't need
    /// churning later.
    #[allow(dead_code)]
    Cancel { xfer_id: u64 },
}

/// Events emitted by the file-transfer service to service.rs.
#[derive(Debug, Clone)]
pub enum Event {
    IncomingOffer {
        xfer_id: u64,
        peer_addr: SocketAddr,
        peer_fingerprint: String,
        root_name: String,
        entries: u32,
        total_bytes: u64,
    },
    Progress {
        xfer_id: u64,
        bytes: u64,
        total: u64,
        entries_done: u32,
        entries_total: u32,
        current_entry: Option<String>,
    },
    Finished {
        xfer_id: u64,
        result: TransferResult,
    },
    /// Counterpart to [`Command::SendClipboard`] — the bytes have arrived
    /// and the service should push them into the local clipboard. No GTK
    /// prompt; this path is silent end-to-end.
    ClipboardReceived {
        peer_addr: SocketAddr,
        content: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
pub enum Decision {
    Accept { dest_dir: PathBuf },
    Decline,
}

#[derive(Debug, Clone)]
pub enum TransferResult {
    Ok { destination: PathBuf },
    Cancelled,
    Error(String),
}

/// Per-transfer caps applied on the receiver before the user sees the offer.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: u32,
}

// ----- Wire protocol -----------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
enum FileCtrl {
    Offer {
        xfer_id: u64,
        root: String,
        entries: u32,
        total_bytes: u64,
        /// Marks the transfer as a silent in-memory clipboard hand-off
        /// rather than a file. Receiver auto-accepts and pipes the bytes
        /// into the local clipboard. Defaults to false so old peers that
        /// never send the field round-trip correctly as ordinary file
        /// offers; new peers omit it from the wire when false to keep
        /// the common-case frame size unchanged.
        #[serde(default, skip_serializing_if = "is_false")]
        clipboard: bool,
    },
    Accept {
        xfer_id: u64,
    },
    Decline {
        xfer_id: u64,
        reason: String,
    },
    Abort {
        xfer_id: u64,
    },
    Progress {
        xfer_id: u64,
        bytes: u64,
        entries_done: u32,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, Deserialize)]
enum FileFrame {
    Entry {
        rel_path: String,
        size: u64,
        mode: u32,
        kind: EntryKind,
        /// If true, each [`FileFrame::Chunk`]'s `data` is zstd-compressed.
        compressed: bool,
    },
    Chunk {
        data: Vec<u8>,
    },
    EntryDone {
        blake3: [u8; 32],
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum EntryKind {
    File,
    Dir,
}

// ----- Service ----------------------------------------------------------

pub struct FileTransferService {
    cmd_tx: mpsc::Sender<Command>,
    event_rx: mpsc::Receiver<Event>,
    endpoint: Endpoint,
    _accept_task: tokio::task::JoinHandle<()>,
}

impl FileTransferService {
    /// Start a file-transfer service on the given bind port. `cert_path`
    /// points at the same on-disk PEM that DTLS uses so both channels share
    /// one identity. `authorized_fingerprints` is the same map backing DTLS
    /// auth — updates made through the GTK frontend apply to both.
    pub async fn start(
        bind_port: u16,
        ipv6_enabled: bool,
        cert_path: &Path,
        authorized_fingerprints: Arc<RwLock<HashMap<String, String>>>,
        limits: Limits,
    ) -> Result<Self, FileTransferError> {
        let (cert_chain, key) = crypto::load_rustls_cert_and_key(cert_path)?;

        // Install the ring crypto provider once, process-wide. Benign if
        // another component already did it.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Listener endpoint
        let bind_addr: SocketAddr = if ipv6_enabled {
            (IpAddr::V6(Ipv6Addr::UNSPECIFIED), bind_port).into()
        } else {
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), bind_port).into()
        };
        let server_config = build_server_config(
            cert_chain.clone(),
            key.clone_key(),
            authorized_fingerprints.clone(),
        )?;
        let endpoint = quinn::Endpoint::server(server_config, bind_addr)?;
        log::info!("file-transfer QUIC listener bound to {bind_addr}");

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(16);
        let (event_tx, event_rx) = mpsc::channel::<Event>(64);

        let accept_task = tokio::task::spawn_local(run_service(
            endpoint.clone(),
            cmd_rx,
            event_tx,
            cert_chain,
            key.clone_key(),
            authorized_fingerprints,
            limits,
        ));

        Ok(Self {
            cmd_tx,
            event_rx,
            endpoint,
            _accept_task: accept_task,
        })
    }

    /// Async variant — used by the loopback test. Production callers
    /// live in a sync select-loop and use [`Self::try_send_command`].
    #[allow(dead_code)]
    pub async fn send_command(&self, cmd: Command) {
        let _ = self.cmd_tx.send(cmd).await;
    }

    /// Non-blocking variant for use from sync handlers (the select-loop
    /// dispatchers). Drops the command and logs if the queue is full —
    /// should never happen in practice since the queue holds 16 entries
    /// and file transfers are rare.
    pub fn try_send_command(&self, cmd: Command) {
        if let Err(e) = self.cmd_tx.try_send(cmd) {
            log::warn!("file-transfer command dropped: {e}");
        }
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        self.event_rx.recv().await
    }

    /// The actual UDP address the QUIC endpoint bound to. Mainly useful in
    /// tests (port 0) and for logging; production callers already know their
    /// configured port.
    #[allow(dead_code)]
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }
}

// ----- Service task ------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_service(
    endpoint: Endpoint,
    mut cmd_rx: mpsc::Receiver<Command>,
    event_tx: mpsc::Sender<Event>,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    authorized_fingerprints: Arc<RwLock<HashMap<String, String>>>,
    limits: Limits,
) {
    let next_id = Arc::new(AtomicU64::new(1));
    let mut pending_offers: HashMap<u64, mpsc::Sender<Decision>> = HashMap::new();
    let mut cancellers: HashMap<u64, tokio::task::JoinHandle<()>> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                None => break,
                Some(Command::SendPath { target_addr, target_fingerprint, root }) => {
                    let xfer_id = next_id.fetch_add(1, Ordering::Relaxed);
                    let event_tx = event_tx.clone();
                    let cert_chain = cert_chain.clone();
                    let key = key.clone_key();
                    let authorized = authorized_fingerprints.clone();
                    let endpoint = endpoint.clone();
                    let handle = tokio::task::spawn_local(async move {
                        let res = send_transfer(
                            endpoint, target_addr, target_fingerprint, cert_chain, key, authorized,
                            xfer_id, root, event_tx.clone(),
                        ).await;
                        let finished = match res {
                            Ok(dest) => Event::Finished {
                                xfer_id,
                                result: TransferResult::Ok { destination: dest },
                            },
                            Err(FileTransferError::Protocol(ref e)) if e == "cancelled" => {
                                Event::Finished {
                                    xfer_id,
                                    result: TransferResult::Cancelled,
                                }
                            }
                            Err(e) => Event::Finished {
                                xfer_id,
                                result: TransferResult::Error(e.to_string()),
                            },
                        };
                        let _ = event_tx.send(finished).await;
                    });
                    cancellers.insert(xfer_id, handle);
                }
                Some(Command::SendClipboard { target_addr, target_fingerprint, content }) => {
                    let xfer_id = next_id.fetch_add(1, Ordering::Relaxed);
                    let event_tx = event_tx.clone();
                    let cert_chain = cert_chain.clone();
                    let key = key.clone_key();
                    let endpoint = endpoint.clone();
                    let handle = tokio::task::spawn_local(async move {
                        let res = send_clipboard_transfer(
                            endpoint, target_addr, target_fingerprint, cert_chain, key,
                            xfer_id, content,
                        ).await;
                        if let Err(e) = res {
                            // Fire a Finished/Error event so a future GTK
                            // status indicator can surface the failure;
                            // the service-side log is the primary signal
                            // for now.
                            let _ = event_tx.send(Event::Finished {
                                xfer_id,
                                result: TransferResult::Error(e.to_string()),
                            }).await;
                        }
                    });
                    cancellers.insert(xfer_id, handle);
                }
                Some(Command::RespondOffer { xfer_id, decision }) => {
                    if let Some(tx) = pending_offers.remove(&xfer_id) {
                        let _ = tx.send(decision).await;
                    } else {
                        log::warn!("RespondOffer for unknown xfer_id={xfer_id}");
                    }
                }
                Some(Command::Cancel { xfer_id }) => {
                    if let Some(handle) = cancellers.remove(&xfer_id) {
                        handle.abort();
                    }
                    if let Some(tx) = pending_offers.remove(&xfer_id) {
                        let _ = tx.send(Decision::Decline).await;
                    }
                }
            },
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let xfer_id = next_id.fetch_add(1, Ordering::Relaxed);
                let (decision_tx, decision_rx) = mpsc::channel::<Decision>(1);
                pending_offers.insert(xfer_id, decision_tx);
                let event_tx = event_tx.clone();
                let handle = tokio::task::spawn_local(async move {
                    let res = recv_transfer(incoming, xfer_id, limits, decision_rx, event_tx.clone()).await;
                    let finished = match res {
                        Ok(RecvOutcome::File(dest)) => Some(Event::Finished {
                            xfer_id,
                            result: TransferResult::Ok { destination: dest },
                        }),
                        Ok(RecvOutcome::Cancelled) => Some(Event::Finished {
                            xfer_id,
                            result: TransferResult::Cancelled,
                        }),
                        // Clipboard transfers emit their own ClipboardReceived
                        // event from inside recv_transfer; suppress the
                        // file-style Finished banner so the GTK UI doesn't
                        // surface a phantom "transfer complete" for what the
                        // user perceives as just clipboard sync.
                        Ok(RecvOutcome::Clipboard) => None,
                        Err(e) => Some(Event::Finished {
                            xfer_id,
                            result: TransferResult::Error(e.to_string()),
                        }),
                    };
                    if let Some(ev) = finished {
                        let _ = event_tx.send(ev).await;
                    }
                });
                cancellers.insert(xfer_id, handle);
            }
        }
    }
}

// ----- Sender ------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn send_transfer(
    endpoint: Endpoint,
    target_addr: SocketAddr,
    target_fingerprint: String,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    _authorized_fingerprints: Arc<RwLock<HashMap<String, String>>>,
    xfer_id: u64,
    root: PathBuf,
    _event_tx: mpsc::Sender<Event>,
) -> Result<PathBuf, FileTransferError> {
    let client_cfg = build_client_config(cert_chain, key, target_fingerprint)?;
    let connecting = endpoint.connect_with(client_cfg, target_addr, "lan-mouse")?;
    let conn = connecting.await?;
    log::debug!("xfer {xfer_id}: connected to {target_addr}");

    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;

    // Walk the tree to size the offer. For single-file transfers this is
    // O(1); for directories it's one metadata call per entry.
    let entries = tokio::task::spawn_blocking({
        let root = root.clone();
        move || enumerate_entries(&root)
    })
    .await
    .map_err(|e| FileTransferError::Protocol(format!("walk panicked: {e}")))??;

    let total_bytes: u64 = entries.iter().map(|e| e.size).sum();
    let root_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("drop")
        .to_string();

    write_frame(
        &mut ctrl_send,
        &FileCtrl::Offer {
            xfer_id,
            root: root_name.clone(),
            entries: entries.len() as u32,
            total_bytes,
            clipboard: false,
        },
    )
    .await?;

    match read_frame::<FileCtrl>(&mut ctrl_recv).await? {
        FileCtrl::Accept { .. } => {}
        FileCtrl::Decline { reason, .. } => {
            return Err(FileTransferError::Protocol(format!("declined: {reason}")));
        }
        other => {
            return Err(FileTransferError::Protocol(format!(
                "unexpected pre-accept frame: {other:?}"
            )));
        }
    }

    for entry in &entries {
        let mut data_send = conn.open_uni().await?;
        send_entry(&mut data_send, entry).await?;
        data_send.finish().ok();
    }

    ctrl_send.finish().ok();
    // Receiver closes the connection once it has processed every stream —
    // we wait here to avoid tearing down mid-read.
    conn.closed().await;
    Ok(root.clone())
}

/// Lightweight sibling of [`send_transfer`] for in-memory clipboard payloads.
/// Same wire format (Offer/Accept + one uni stream with Entry/Chunk/EntryDone)
/// but the Offer carries `clipboard: true`, so the receiver auto-accepts
/// without prompting and pipes the bytes into its local clipboard rather than
/// writing a file. No filesystem activity on either side.
async fn send_clipboard_transfer(
    endpoint: Endpoint,
    target_addr: SocketAddr,
    target_fingerprint: String,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    xfer_id: u64,
    content: Vec<u8>,
) -> Result<(), FileTransferError> {
    let total_bytes = content.len() as u64;
    let client_cfg = build_client_config(cert_chain, key, target_fingerprint)?;
    let connecting = endpoint.connect_with(client_cfg, target_addr, "lan-mouse")?;
    let conn = connecting.await?;
    log::debug!("clipboard xfer {xfer_id}: connected to {target_addr} ({total_bytes} bytes)");

    let (mut ctrl_send, mut ctrl_recv) = conn.open_bi().await?;
    write_frame(
        &mut ctrl_send,
        &FileCtrl::Offer {
            xfer_id,
            root: "clipboard".into(),
            entries: 1,
            total_bytes,
            clipboard: true,
        },
    )
    .await?;

    match read_frame::<FileCtrl>(&mut ctrl_recv).await? {
        FileCtrl::Accept { .. } => {}
        FileCtrl::Decline { reason, .. } => {
            return Err(FileTransferError::Protocol(format!(
                "clipboard declined: {reason}"
            )));
        }
        other => {
            return Err(FileTransferError::Protocol(format!(
                "unexpected pre-accept frame: {other:?}"
            )));
        }
    }

    let mut data_send = conn.open_uni().await?;
    let hash: [u8; 32] = blake3::hash(&content).into();
    write_frame(
        &mut data_send,
        &FileFrame::Entry {
            rel_path: "clipboard".into(),
            size: total_bytes,
            mode: 0,
            kind: EntryKind::File,
            // Skip zstd: clipboard payloads are typically small text where
            // the framing overhead would dominate any compression win, and
            // skipping keeps this path branch-free with the existing chunker.
            compressed: false,
        },
    )
    .await?;
    // One Chunk fits the worst case: clipboard is bounded above by
    // MAX_CLIPBOARD_SIZE (64 KB) which is half of MAX_FRAME_BYTES (128 KB).
    if !content.is_empty() {
        write_frame(&mut data_send, &FileFrame::Chunk { data: content }).await?;
    }
    write_frame(&mut data_send, &FileFrame::EntryDone { blake3: hash }).await?;
    data_send.finish().ok();
    ctrl_send.finish().ok();
    conn.closed().await;
    Ok(())
}

#[derive(Debug, Clone)]
struct LocalEntry {
    rel_path: String,
    absolute: PathBuf,
    size: u64,
    mode: u32,
    is_dir: bool,
}

fn enumerate_entries(root: &Path) -> Result<Vec<LocalEntry>, FileTransferError> {
    let meta = std::fs::symlink_metadata(root)?;
    let mut out = Vec::new();
    let root_display = root
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("drop"));
    if meta.is_file() {
        out.push(LocalEntry {
            rel_path: root_display.to_string_lossy().into_owned(),
            absolute: root.to_path_buf(),
            size: meta.len(),
            mode: unix_mode(&meta),
            is_dir: false,
        });
        return Ok(out);
    }
    if !meta.is_dir() {
        // symlink or other special; skip for v1
        return Ok(out);
    }
    walk_dir(root, &root_display, &mut out)?;
    Ok(out)
}

fn walk_dir(
    abs: &Path,
    rel_prefix: &Path,
    out: &mut Vec<LocalEntry>,
) -> Result<(), FileTransferError> {
    let meta = std::fs::symlink_metadata(abs)?;
    out.push(LocalEntry {
        rel_path: rel_prefix.to_string_lossy().into_owned(),
        absolute: abs.to_path_buf(),
        size: 0,
        mode: unix_mode(&meta),
        is_dir: true,
    });
    for entry in std::fs::read_dir(abs)? {
        let entry = entry?;
        let entry_meta = entry.metadata()?;
        let entry_name = entry.file_name();
        let new_rel = rel_prefix.join(&entry_name);
        let entry_abs = abs.join(&entry_name);
        if entry_meta.is_dir() {
            walk_dir(&entry_abs, &new_rel, out)?;
        } else if entry_meta.is_file() {
            out.push(LocalEntry {
                rel_path: new_rel.to_string_lossy().into_owned(),
                absolute: entry_abs,
                size: entry_meta.len(),
                mode: unix_mode(&entry_meta),
                is_dir: false,
            });
        }
        // symlinks and special files are skipped for v1
    }
    Ok(())
}

#[cfg(unix)]
fn unix_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn unix_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

async fn send_entry(send: &mut SendStream, entry: &LocalEntry) -> Result<(), FileTransferError> {
    if entry.is_dir {
        write_frame(
            send,
            &FileFrame::Entry {
                rel_path: entry.rel_path.clone(),
                size: 0,
                mode: entry.mode,
                kind: EntryKind::Dir,
                compressed: false,
            },
        )
        .await?;
        write_frame(
            send,
            &FileFrame::EntryDone {
                blake3: blake3::Hasher::new().finalize().into(),
            },
        )
        .await?;
        return Ok(());
    }

    let mut file = tokio::fs::File::open(&entry.absolute).await?;
    let mut head = [0u8; 16];
    let head_len = {
        let mut got = 0;
        while got < head.len() {
            let n = file.read(&mut head[got..]).await?;
            if n == 0 {
                break;
            }
            got += n;
        }
        got
    };
    let compressed = !is_already_compressed(&head[..head_len]);
    write_frame(
        send,
        &FileFrame::Entry {
            rel_path: entry.rel_path.clone(),
            size: entry.size,
            mode: entry.mode,
            kind: EntryKind::File,
            compressed,
        },
    )
    .await?;

    let mut hasher = blake3::Hasher::new();
    // Emit the already-read head as the first chunk.
    if head_len > 0 {
        hasher.update(&head[..head_len]);
        let payload = maybe_compress(&head[..head_len], compressed)?;
        write_frame(send, &FileFrame::Chunk { data: payload }).await?;
    }

    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        let payload = maybe_compress(&buf[..n], compressed)?;
        write_frame(send, &FileFrame::Chunk { data: payload }).await?;
    }

    write_frame(
        send,
        &FileFrame::EntryDone {
            blake3: *hasher.finalize().as_bytes(),
        },
    )
    .await?;
    Ok(())
}

fn maybe_compress(raw: &[u8], compress: bool) -> Result<Vec<u8>, FileTransferError> {
    if !compress {
        return Ok(raw.to_vec());
    }
    zstd::bulk::compress(raw, 3).map_err(FileTransferError::Io)
}

fn maybe_decompress(data: Vec<u8>, was_compressed: bool) -> Result<Vec<u8>, FileTransferError> {
    if !was_compressed {
        return Ok(data);
    }
    zstd::bulk::decompress(&data, CHUNK_SIZE * 2).map_err(FileTransferError::Io)
}

fn is_already_compressed(head: &[u8]) -> bool {
    ALREADY_COMPRESSED_MAGICS
        .iter()
        .any(|m| head.len() >= m.len() && &head[..m.len()] == *m)
}

// ----- Receiver ----------------------------------------------------------

/// What `recv_transfer` resolved to. Lets the outer service loop decide
/// whether to emit a final `Event::Finished` (skipped for clipboard
/// transfers since they emit their own [`Event::ClipboardReceived`] and
/// should not surface a redundant transfer-completed banner in the UI).
enum RecvOutcome {
    File(PathBuf),
    Cancelled,
    Clipboard,
}

async fn recv_transfer(
    incoming: quinn::Incoming,
    xfer_id: u64,
    limits: Limits,
    mut decision_rx: mpsc::Receiver<Decision>,
    event_tx: mpsc::Sender<Event>,
) -> Result<RecvOutcome, FileTransferError> {
    let conn = incoming.await?;
    let peer_addr = conn.remote_address();
    let peer_fp = peer_fingerprint(&conn);
    log::debug!("xfer {xfer_id}: accepted from {peer_addr} fp={peer_fp}");

    let (mut ctrl_send, mut ctrl_recv) = conn.accept_bi().await?;
    let (root_name, entries_total, total_bytes, is_clipboard) =
        match read_frame::<FileCtrl>(&mut ctrl_recv).await? {
            FileCtrl::Offer {
                xfer_id: _,
                root,
                entries,
                total_bytes,
                clipboard,
            } => (root, entries, total_bytes, clipboard),
            other => {
                return Err(FileTransferError::Protocol(format!(
                    "first ctrl frame was not Offer: {other:?}"
                )));
            }
        };

    if total_bytes > limits.max_bytes || entries_total > limits.max_entries {
        write_frame(
            &mut ctrl_send,
            &FileCtrl::Decline {
                xfer_id,
                reason: "offer exceeds configured transfer limits".into(),
            },
        )
        .await?;
        ctrl_send.finish().ok();
        return Ok(RecvOutcome::Cancelled);
    }

    if is_clipboard {
        // Auto-accept silent clipboard transfer. No GTK prompt, no disk I/O.
        // We still go through the same Entry/Chunk/EntryDone framing so the
        // sender can reuse the chunker and so a future "large clipboard"
        // case (multi-chunk) just works.
        write_frame(&mut ctrl_send, &FileCtrl::Accept { xfer_id }).await?;
        let mut data_recv = conn.accept_uni().await?;
        let (compressed, expected_size) = match read_frame::<FileFrame>(&mut data_recv).await? {
            FileFrame::Entry {
                kind: EntryKind::File,
                size,
                compressed,
                ..
            } => (compressed, size),
            other => {
                return Err(FileTransferError::Protocol(format!(
                    "clipboard: first data frame was not File Entry: {other:?}"
                )));
            }
        };
        let mut buf: Vec<u8> = Vec::with_capacity(expected_size as usize);
        let mut hasher = blake3::Hasher::new();
        let expected_hash;
        loop {
            match read_frame::<FileFrame>(&mut data_recv).await? {
                FileFrame::Chunk { data } => {
                    let raw = maybe_decompress(data, compressed)?;
                    hasher.update(&raw);
                    buf.extend_from_slice(&raw);
                }
                FileFrame::EntryDone { blake3 } => {
                    expected_hash = blake3;
                    break;
                }
                other => {
                    return Err(FileTransferError::Protocol(format!(
                        "clipboard: expected Chunk/EntryDone, got: {other:?}"
                    )));
                }
            }
        }
        let actual: [u8; 32] = *hasher.finalize().as_bytes();
        if actual != expected_hash {
            return Err(FileTransferError::Protocol(
                "clipboard: blake3 mismatch".into(),
            ));
        }
        ctrl_send.finish().ok();
        conn.close(0u32.into(), b"done");
        log::debug!(
            "clipboard xfer {xfer_id}: received {} bytes from {peer_addr}",
            buf.len()
        );
        event_tx
            .send(Event::ClipboardReceived {
                peer_addr,
                content: buf,
            })
            .await
            .ok();
        return Ok(RecvOutcome::Clipboard);
    }

    event_tx
        .send(Event::IncomingOffer {
            xfer_id,
            peer_addr,
            peer_fingerprint: peer_fp,
            root_name: root_name.clone(),
            entries: entries_total,
            total_bytes,
        })
        .await
        .ok();

    let decision = match decision_rx.recv().await {
        Some(d) => d,
        None => Decision::Decline,
    };
    let dest_dir = match decision {
        Decision::Accept { dest_dir } => dest_dir,
        Decision::Decline => {
            write_frame(
                &mut ctrl_send,
                &FileCtrl::Decline {
                    xfer_id,
                    reason: "user declined".into(),
                },
            )
            .await?;
            ctrl_send.finish().ok();
            return Ok(RecvOutcome::Cancelled);
        }
    };

    write_frame(&mut ctrl_send, &FileCtrl::Accept { xfer_id }).await?;

    // Destination root: <dest_dir>/<root_name>. Create it now so the user can
    // tell where the files will land even mid-transfer.
    let root_dst = dest_dir.join(sanitize_component(&root_name));
    tokio::fs::create_dir_all(&root_dst).await.ok();

    let mut bytes_done: u64 = 0;
    let mut entries_done: u32 = 0;
    let mut last_progress = Instant::now();
    let mut last_progress_bytes: u64 = 0;
    let mut part_files: Vec<PathBuf> = Vec::new();

    let abort_cleanup = |parts: &[PathBuf]| {
        for p in parts {
            let _ = std::fs::remove_file(p);
        }
    };

    while entries_done < entries_total {
        let mut data_recv = match conn.accept_uni().await {
            Ok(r) => r,
            Err(e) => {
                abort_cleanup(&part_files);
                return Err(FileTransferError::Connection(e));
            }
        };

        let (entry_kind, rel_path, size, mode, compressed) =
            match read_frame::<FileFrame>(&mut data_recv).await? {
                FileFrame::Entry {
                    kind,
                    rel_path,
                    size,
                    mode,
                    compressed,
                } => (kind, rel_path, size, mode, compressed),
                other => {
                    abort_cleanup(&part_files);
                    return Err(FileTransferError::Protocol(format!(
                        "first data frame was not Entry: {other:?}"
                    )));
                }
            };

        // Resolve & validate. Reject absolute components, `..`, and any
        // canonical path that escapes root_dst.
        let final_path = match safe_join(&root_dst, &rel_path) {
            Some(p) => p,
            None => {
                abort_cleanup(&part_files);
                return Err(FileTransferError::Protocol(format!(
                    "rejecting unsafe rel_path: {rel_path:?}"
                )));
            }
        };

        match entry_kind {
            EntryKind::Dir => {
                tokio::fs::create_dir_all(&final_path).await.ok();
                match read_frame::<FileFrame>(&mut data_recv).await? {
                    FileFrame::EntryDone { .. } => {}
                    other => {
                        abort_cleanup(&part_files);
                        return Err(FileTransferError::Protocol(format!(
                            "expected EntryDone after Dir Entry, got: {other:?}"
                        )));
                    }
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = tokio::fs::set_permissions(
                        &final_path,
                        std::fs::Permissions::from_mode(mode & 0o777),
                    )
                    .await;
                }
                #[cfg(not(unix))]
                let _ = mode; // unused on non-unix
            }
            EntryKind::File => {
                if let Some(parent) = final_path.parent() {
                    tokio::fs::create_dir_all(parent).await.ok();
                }
                let part_path = {
                    let mut p = final_path.clone();
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "drop".into());
                    p.set_file_name(format!("{name}.lanmouse.part"));
                    p
                };
                part_files.push(part_path.clone());
                let mut file = tokio::fs::File::create(&part_path).await?;
                let mut hasher = blake3::Hasher::new();

                let expected_hash;
                loop {
                    match read_frame::<FileFrame>(&mut data_recv).await? {
                        FileFrame::Chunk { data } => {
                            let raw = maybe_decompress(data, compressed)?;
                            hasher.update(&raw);
                            file.write_all(&raw).await?;
                            bytes_done = bytes_done.saturating_add(raw.len() as u64);

                            let now = Instant::now();
                            if now.duration_since(last_progress) >= PROGRESS_INTERVAL
                                || bytes_done - last_progress_bytes >= PROGRESS_BYTES
                            {
                                event_tx
                                    .send(Event::Progress {
                                        xfer_id,
                                        bytes: bytes_done,
                                        total: total_bytes,
                                        entries_done,
                                        entries_total,
                                        current_entry: Some(rel_path.clone()),
                                    })
                                    .await
                                    .ok();
                                last_progress = now;
                                last_progress_bytes = bytes_done;
                            }
                        }
                        FileFrame::EntryDone { blake3 } => {
                            expected_hash = blake3;
                            break;
                        }
                        other => {
                            abort_cleanup(&part_files);
                            return Err(FileTransferError::Protocol(format!(
                                "expected Chunk/EntryDone, got: {other:?}"
                            )));
                        }
                    }
                }

                file.flush().await?;
                drop(file);

                let actual: [u8; 32] = *hasher.finalize().as_bytes();
                if actual != expected_hash {
                    abort_cleanup(&part_files);
                    return Err(FileTransferError::Protocol(format!(
                        "blake3 mismatch on {rel_path}"
                    )));
                }

                tokio::fs::rename(&part_path, &final_path).await?;
                part_files.pop();
                let _ = size; // size is informational; blake3 is authoritative
            }
        }

        entries_done += 1;
    }

    ctrl_send.finish().ok();
    conn.close(0u32.into(), b"done");
    Ok(RecvOutcome::File(root_dst))
}

fn peer_fingerprint(conn: &quinn::Connection) -> String {
    if let Some(pi) = conn.peer_identity() {
        if let Ok(chain) = pi.downcast::<Vec<CertificateDer<'static>>>() {
            if let Some(cert) = chain.first() {
                return crypto::generate_fingerprint(cert.as_ref());
            }
        }
    }
    String::new()
}

/// Join `root` with `rel` only if the result stays inside `root`. Rejects
/// absolute components and any `..` that escapes.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let mut out = root.to_path_buf();
    let mut depth: i32 = 0;
    for comp in p.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => return None,
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                out.pop();
            }
            Component::CurDir => {}
            Component::Normal(part) => {
                depth += 1;
                out.push(sanitize_component(&part.to_string_lossy()));
            }
        }
    }
    Some(out)
}

/// Strip nul bytes, trim whitespace, reject empty / reserved names. Keeps
/// path traversal at the structural level (see [`safe_join`]); this is just
/// per-component hygiene.
fn sanitize_component(raw: &str) -> String {
    let s = raw.replace('\0', "").trim().to_string();
    if s.is_empty() || s == "." || s == ".." {
        "_".into()
    } else {
        s
    }
}

// ----- Framing ----------------------------------------------------------

async fn write_frame<T: Serialize>(
    send: &mut SendStream,
    value: &T,
) -> Result<(), FileTransferError> {
    let mut body = Vec::new();
    ciborium::into_writer(value, &mut body)?;
    let len = body.len() as u32;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

async fn read_frame<T: for<'de> Deserialize<'de>>(
    recv: &mut RecvStream,
) -> Result<T, FileTransferError> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FileTransferError::Protocol(format!(
            "frame too large ({len} > {MAX_FRAME_BYTES})"
        )));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(ciborium::from_reader(body.as_slice())?)
}

// ----- TLS configs ------------------------------------------------------

fn build_server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    authorized: Arc<RwLock<HashMap<String, String>>>,
) -> Result<quinn::ServerConfig, FileTransferError> {
    let verifier = Arc::new(FingerprintClientVerifier { authorized });
    let crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_client_cert_verifier(verifier)
    .with_single_cert(cert_chain, key)?;
    let quic = QuicServerConfig::try_from(crypto)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic)))
}

fn build_client_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    expected_peer_fp: String,
) -> Result<quinn::ClientConfig, FileTransferError> {
    let verifier = Arc::new(FingerprintServerVerifier { expected_peer_fp });
    let crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(verifier)
    .with_client_auth_cert(cert_chain, key)?;
    let quic = QuicClientConfig::try_from(crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

#[derive(Debug)]
struct FingerprintClientVerifier {
    authorized: Arc<RwLock<HashMap<String, String>>>,
}

impl ClientCertVerifier for FingerprintClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let fp = crypto::generate_fingerprint(end_entity.as_ref());
        let allowed = self
            .authorized
            .read()
            .ok()
            .map(|m| m.contains_key(&fp))
            .unwrap_or(false);
        if allowed {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "client fingerprint {fp} not authorized"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        default_schemes()
    }
}

#[derive(Debug)]
struct FingerprintServerVerifier {
    expected_peer_fp: String,
}

impl ServerCertVerifier for FingerprintServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = crypto::generate_fingerprint(end_entity.as_ref());
        if fp == self.expected_peer_fp {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "server fingerprint {fp} does not match expected {}",
                self.expected_peer_fp
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        default_schemes()
    }
}

fn default_schemes() -> Vec<SignatureScheme> {
    vec![
        SignatureScheme::ED25519,
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::RSA_PSS_SHA256,
        SignatureScheme::RSA_PSS_SHA384,
        SignatureScheme::RSA_PSS_SHA512,
    ]
}

// ----- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_accepts_nested() {
        let root = Path::new("/tmp/lm-test");
        assert_eq!(safe_join(root, "a/b/c").unwrap(), root.join("a/b/c"));
    }

    #[test]
    fn safe_join_rejects_parent_escape() {
        let root = Path::new("/tmp/lm-test");
        assert!(safe_join(root, "../outside").is_none());
        assert!(safe_join(root, "a/../../outside").is_none());
    }

    #[test]
    fn safe_join_rejects_absolute() {
        let root = Path::new("/tmp/lm-test");
        assert!(safe_join(root, "/etc/passwd").is_none());
    }

    #[test]
    fn safe_join_collapses_self() {
        let root = Path::new("/tmp/lm-test");
        assert_eq!(safe_join(root, "a/./b").unwrap(), root.join("a/b"));
    }

    #[test]
    fn is_already_compressed_detects_png() {
        assert!(is_already_compressed(b"\x89PNG\r\n\x1a\nextra"));
        assert!(!is_already_compressed(b"#!/bin/bash"));
    }

    #[test]
    fn sanitize_component_strips_nuls() {
        assert_eq!(sanitize_component("foo\0bar"), "foobar");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component(".."), "_");
    }

    /// End-to-end smoke test: two FileTransferService instances on loopback
    /// round-trip a small file. Validates the QUIC handshake, fingerprint
    /// verification, CBOR framing, chunk + EntryDone flow, and destination
    /// rename — i.e. the critical path of task 8 before task 11 builds on it.
    #[test]
    fn loopback_roundtrip_small_file() {
        use std::collections::HashMap;
        use std::sync::{Arc, RwLock};
        use tokio::runtime::Builder;
        use tokio::task::LocalSet;

        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let local = LocalSet::new();
        runtime.block_on(local.run_until(async move {
            // Unique scratch dir so parallel runs don't collide.
            let scratch = std::env::temp_dir().join(format!(
                "lan-mouse-ft-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&scratch).unwrap();

            let cert_path = scratch.join("identity.pem");
            let cert = crypto::generate_key_and_cert(&cert_path).unwrap();
            let fp = crypto::certificate_fingerprint(&cert);

            // Both sides use the same identity. A real deployment has
            // distinct certs but the fingerprint-allowlist verifier doesn't
            // care — it just checks the map.
            let authorized = Arc::new(RwLock::new({
                let mut m = HashMap::new();
                m.insert(fp.clone(), "loopback".into());
                m
            }));

            let limits = Limits {
                max_bytes: 10 * 1024 * 1024,
                max_entries: 100,
            };

            let mut receiver =
                FileTransferService::start(0, false, &cert_path, authorized.clone(), limits)
                    .await
                    .expect("receiver start");
            let sender =
                FileTransferService::start(0, false, &cert_path, authorized.clone(), limits)
                    .await
                    .expect("sender start");
            let recv_port = receiver.local_addr().unwrap().port();

            let source_path = scratch.join("hello.txt");
            let payload = b"hello from quic land\n".repeat(1000);
            std::fs::write(&source_path, &payload).unwrap();

            let dest_dir = scratch.join("inbox");
            std::fs::create_dir_all(&dest_dir).unwrap();

            // Initiate send.
            sender
                .send_command(Command::SendPath {
                    target_addr: (std::net::Ipv4Addr::LOCALHOST, recv_port).into(),
                    target_fingerprint: fp.clone(),
                    root: source_path.clone(),
                })
                .await;

            // Receiver observes the offer; respond with Accept.
            let offer = tokio::time::timeout(Duration::from_secs(5), receiver.next_event())
                .await
                .expect("offer within 5s")
                .expect("receiver got event");
            let xfer_id = match offer {
                Event::IncomingOffer { xfer_id, .. } => xfer_id,
                other => panic!("unexpected first event: {other:?}"),
            };

            receiver
                .send_command(Command::RespondOffer {
                    xfer_id,
                    decision: Decision::Accept {
                        dest_dir: dest_dir.clone(),
                    },
                })
                .await;

            // Drain events until the receiver reports Finished. Non-Finished
            // variants either loop (Progress) or panic; falling off the loop
            // via `break` is proof the transfer completed cleanly.
            loop {
                match tokio::time::timeout(Duration::from_secs(10), receiver.next_event()).await {
                    Ok(Some(Event::Finished {
                        xfer_id: fin_id,
                        result: TransferResult::Ok { .. },
                    })) => {
                        assert_eq!(fin_id, xfer_id);
                        break;
                    }
                    Ok(Some(Event::Finished {
                        result: TransferResult::Error(e),
                        ..
                    })) => panic!("receiver reported error: {e}"),
                    Ok(Some(Event::Finished {
                        result: TransferResult::Cancelled,
                        ..
                    })) => panic!("receiver reported cancel"),
                    Ok(Some(Event::Progress { .. })) => {}
                    Ok(Some(other)) => panic!("unexpected event: {other:?}"),
                    Ok(None) => panic!("receiver channel closed early"),
                    Err(_) => panic!("timeout waiting for Finished"),
                }
            }

            let delivered = dest_dir.join("hello.txt").join("hello.txt");
            // ^ root_name is "hello.txt" so dest is inbox/hello.txt/<entry>.
            //   The entry's own rel_path is also "hello.txt" (the file name).
            //   This matches enumerate_entries' single-file handling which
            //   uses the root's file_name as the rel_path.
            let got = std::fs::read(&delivered)
                .unwrap_or_else(|e| panic!("expected delivered file at {delivered:?}: {e}"));
            assert_eq!(got, payload, "payload mismatch");

            // Best-effort cleanup.
            let _ = std::fs::remove_dir_all(&scratch);
        }));
    }

    /// Decline path: receiver responds Decline, no file is written, the
    /// receiver reports Cancelled, and the destination directory stays
    /// empty. Covers the user-rejects-the-offer cleanup path.
    #[test]
    fn loopback_decline_rejects_cleanly() {
        use std::collections::HashMap;
        use std::sync::{Arc, RwLock};
        use tokio::runtime::Builder;
        use tokio::task::LocalSet;

        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let local = LocalSet::new();
        runtime.block_on(local.run_until(async move {
            let scratch = std::env::temp_dir().join(format!(
                "lan-mouse-ft-decline-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&scratch).unwrap();

            let cert_path = scratch.join("identity.pem");
            let cert = crypto::generate_key_and_cert(&cert_path).unwrap();
            let fp = crypto::certificate_fingerprint(&cert);

            let authorized = Arc::new(RwLock::new({
                let mut m = HashMap::new();
                m.insert(fp.clone(), "loopback".into());
                m
            }));
            let limits = Limits {
                max_bytes: 1024 * 1024,
                max_entries: 100,
            };

            let mut receiver =
                FileTransferService::start(0, false, &cert_path, authorized.clone(), limits)
                    .await
                    .expect("receiver start");
            let sender =
                FileTransferService::start(0, false, &cert_path, authorized.clone(), limits)
                    .await
                    .expect("sender start");
            let recv_port = receiver.local_addr().unwrap().port();

            let source_path = scratch.join("decline-me.txt");
            std::fs::write(&source_path, b"rejected payload").unwrap();
            let dest_dir = scratch.join("should-stay-empty");
            std::fs::create_dir_all(&dest_dir).unwrap();

            sender
                .send_command(Command::SendPath {
                    target_addr: (std::net::Ipv4Addr::LOCALHOST, recv_port).into(),
                    target_fingerprint: fp.clone(),
                    root: source_path.clone(),
                })
                .await;

            // Drain until we see the offer, then decline.
            let xfer_id = loop {
                match tokio::time::timeout(Duration::from_secs(5), receiver.next_event()).await {
                    Ok(Some(Event::IncomingOffer { xfer_id, .. })) => break xfer_id,
                    Ok(Some(_)) => continue,
                    Ok(None) => panic!("receiver channel closed"),
                    Err(_) => panic!("timeout waiting for offer"),
                }
            };
            receiver
                .send_command(Command::RespondOffer {
                    xfer_id,
                    decision: Decision::Decline,
                })
                .await;

            // Receiver must emit Finished{Cancelled} after the decline.
            let mut got_terminal = false;
            for _ in 0..10 {
                match tokio::time::timeout(Duration::from_secs(5), receiver.next_event()).await {
                    Ok(Some(Event::Finished {
                        result: TransferResult::Cancelled,
                        ..
                    })) => {
                        got_terminal = true;
                        break;
                    }
                    Ok(Some(_)) => continue,
                    _ => break,
                }
            }
            assert!(
                got_terminal,
                "expected Cancelled terminal event on receiver"
            );

            // Destination directory must have no files created.
            let entries: Vec<_> = std::fs::read_dir(&dest_dir).unwrap().collect();
            assert!(
                entries.is_empty(),
                "decline should leave destination empty, got {} entries",
                entries.len()
            );

            let _ = std::fs::remove_dir_all(&scratch);
        }));
    }

    /// Silent clipboard hand-off: sender ships an in-memory payload with
    /// `SendClipboard`, receiver auto-accepts (no GTK prompt expected) and
    /// emits a `ClipboardReceived` event with the original bytes intact.
    /// Regression-guards both the new wire field on `FileCtrl::Offer` and
    /// the in-memory accumulator path inside `recv_transfer`.
    #[test]
    fn loopback_silent_clipboard_roundtrip() {
        use std::collections::HashMap;
        use std::sync::{Arc, RwLock};
        use tokio::runtime::Builder;
        use tokio::task::LocalSet;

        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let local = LocalSet::new();
        runtime.block_on(local.run_until(async move {
            let scratch = std::env::temp_dir().join(format!(
                "lan-mouse-ft-clip-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&scratch).unwrap();

            let cert_path = scratch.join("identity.pem");
            let cert = crypto::generate_key_and_cert(&cert_path).unwrap();
            let fp = crypto::certificate_fingerprint(&cert);

            let authorized = Arc::new(RwLock::new({
                let mut m = HashMap::new();
                m.insert(fp.clone(), "loopback".into());
                m
            }));
            let limits = Limits {
                max_bytes: 1024 * 1024,
                max_entries: 100,
            };

            let mut receiver =
                FileTransferService::start(0, false, &cert_path, authorized.clone(), limits)
                    .await
                    .expect("receiver start");
            let sender =
                FileTransferService::start(0, false, &cert_path, authorized.clone(), limits)
                    .await
                    .expect("sender start");
            let recv_port = receiver.local_addr().unwrap().port();

            // ~9 KB payload — well past the 1 KB inline DTLS cap and past
            // the Windows default UDP recv buffer that triggered the
            // original WSAEMSGSIZE wedge.
            let payload: Vec<u8> = (0..9000).map(|i| (i % 251) as u8).collect();

            sender
                .send_command(Command::SendClipboard {
                    target_addr: (std::net::Ipv4Addr::LOCALHOST, recv_port).into(),
                    target_fingerprint: fp.clone(),
                    content: payload.clone(),
                })
                .await;

            let event = tokio::time::timeout(Duration::from_secs(10), receiver.next_event())
                .await
                .expect("clipboard event within 10s")
                .expect("receiver got event");
            match event {
                Event::ClipboardReceived { content, .. } => {
                    assert_eq!(content, payload, "clipboard payload mismatch");
                }
                Event::IncomingOffer { .. } => {
                    panic!("silent clipboard must not surface a user-prompt IncomingOffer")
                }
                other => panic!("unexpected event: {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&scratch);
        }));
    }
}
