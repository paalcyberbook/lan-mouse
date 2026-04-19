use std::{
    collections::{HashMap, HashSet},
    env::VarError,
    fmt::Display,
    io,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};
use thiserror::Error;

#[cfg(unix)]
use std::{env, path::Path};

use serde::{Deserialize, Serialize};

mod connect;
mod connect_async;
mod listen;

pub use connect::{FrontendEventReader, FrontendRequestWriter, connect};
pub use connect_async::{AsyncFrontendEventReader, AsyncFrontendRequestWriter, connect_async};
pub use listen::AsyncFrontendListener;

#[derive(Debug, Error)]
pub enum ConnectionError {
    #[error(transparent)]
    SocketPath(#[from] SocketPathError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("connection timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum IpcListenerCreationError {
    #[error("could not determine socket-path: `{0}`")]
    SocketPath(#[from] SocketPathError),
    #[error("service already running!")]
    AlreadyRunning,
    #[error("failed to bind lan-mouse socket: `{0}`")]
    Bind(io::Error),
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("io error occured: `{0}`")]
    Io(#[from] io::Error),
    #[error("invalid json: `{0}`")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] ConnectionError),
    #[error(transparent)]
    Listen(#[from] IpcListenerCreationError),
}

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Debug, Default, Eq, Hash, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Position::Right,
            Position::Right => Position::Left,
            Position::Top => Position::Bottom,
            Position::Bottom => Position::Top,
        }
    }
}

#[derive(Debug, Error)]
#[error("not a valid position: {pos}")]
pub struct PositionParseError {
    pos: String,
}

impl FromStr for Position {
    type Err = PositionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "top" => Ok(Self::Top),
            "bottom" => Ok(Self::Bottom),
            _ => Err(PositionParseError { pos: s.into() }),
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Position::Left => "left",
                Position::Right => "right",
                Position::Top => "top",
                Position::Bottom => "bottom",
            }
        )
    }
}

impl TryFrom<&str> for Position {
    type Error = ();

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "left" => Ok(Position::Left),
            "right" => Ok(Position::Right),
            "top" => Ok(Position::Top),
            "bottom" => Ok(Position::Bottom),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// hostname of this client
    pub hostname: Option<String>,
    /// fix ips, determined by the user
    pub fix_ips: Vec<IpAddr>,
    /// both active_addr and addrs can be None / empty so port needs to be stored seperately
    pub port: u16,
    /// position of a client on screen
    pub pos: Position,
    /// enter hook
    pub cmd: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            hostname: Default::default(),
            fix_ips: Default::default(),
            pos: Default::default(),
            cmd: None,
        }
    }
}

pub type ClientHandle = u64;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientState {
    /// events should be sent to and received from the client
    pub active: bool,
    /// `active` address of the client, used to send data to.
    /// This should generally be the socket address where data
    /// was last received from.
    pub active_addr: Option<SocketAddr>,
    /// tracks whether or not the client is available for emulation
    pub alive: bool,
    /// ips from dns
    pub dns_ips: Vec<IpAddr>,
    /// all ip addresses associated with a particular client
    /// e.g. Laptops usually have at least an ethernet and a wifi port
    /// which have different ip addresses
    pub ips: HashSet<IpAddr>,
    /// client has pressed keys
    pub has_pressed_keys: bool,
    /// dns resolving in progress
    pub resolving: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    /// a client was created
    Created(ClientHandle, ClientConfig, ClientState),
    /// no such client
    NoSuchClient(ClientHandle),
    /// state changed
    State(ClientHandle, ClientConfig, ClientState),
    /// the client was deleted
    Deleted(ClientHandle),
    /// new port, reason of failure (if failed)
    PortChanged(u16, Option<String>),
    /// list of all clients, used for initial state synchronization
    Enumerate(Vec<(ClientHandle, ClientConfig, ClientState)>),
    /// an error occured
    Error(String),
    /// capture status
    CaptureStatus(Status),
    /// emulation status
    EmulationStatus(Status),
    /// authorized public key fingerprints have been updated
    AuthorizedUpdated(HashMap<String, String>),
    /// public key fingerprint of this device
    PublicKeyFingerprint(String),
    /// new device connected
    DeviceConnected {
        addr: SocketAddr,
        fingerprint: String,
    },
    /// incoming device entered the screen
    DeviceEntered {
        fingerprint: String,
        addr: SocketAddr,
        pos: Position,
    },
    /// incoming disconnected
    IncomingDisconnected(SocketAddr),
    /// failed connection attempt (approval for fingerprint required)
    ConnectionAttempt { fingerprint: String },
    /// a device was discovered on the network via mDNS
    DiscoveredDevice {
        hostname: String,
        addrs: Vec<IpAddr>,
        port: u16,
        fingerprint: String,
        /// the position the discoverer configured for us — we suggest the opposite
        position: Position,
    },
    /// a previously discovered device is no longer available
    DeviceLost { hostname: String },
    /// discoverable mode changed
    DiscoverableChanged(bool),
    /// settings changed
    SettingsChanged(Settings),
    /// incoming file / folder transfer offer awaiting user decision on the receiver
    FileOfferIncoming {
        xfer_id: u64,
        client: ClientHandle,
        root_name: String,
        entries: u32,
        total_bytes: u64,
        fingerprint: String,
    },
    /// progress update for an accepted transfer (receiver-side UI)
    FileTransferProgress {
        xfer_id: u64,
        bytes: u64,
        total: u64,
        entries_done: u32,
        entries_total: u32,
        current_entry: Option<String>,
    },
    /// a transfer terminated (success, user cancel, or error)
    FileTransferFinished {
        xfer_id: u64,
        result: TransferResult,
    },
    /// the local clipboard text exceeded the 64 KiB sync cap; ask the sender
    /// whether to ship it as a file instead
    ClipboardOverflow {
        client: ClientHandle,
        bytes: u64,
        preview: String,
    },
    /// an image was placed on the local clipboard; ask the sender whether to
    /// encode it as PNG and ship it
    ClipboardImageDetected {
        client: ClientHandle,
        bytes: u64,
        width: u32,
        height: u32,
    },
}

/// Sender's decision on an incoming `FileOfferIncoming` — accept with a
/// destination directory, or decline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileDecision {
    Accept { dest_dir: PathBuf },
    Decline,
}

/// Terminal state of a file transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferResult {
    Ok { destination: PathBuf },
    Cancelled,
    Error(String),
}

/// Sender's decision when clipboard text exceeds the 64 KiB cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardOverflowDecision {
    /// ship the full payload as a file via the file-transfer channel
    Send,
    /// keep existing behavior: truncate to 64 KiB and sync as text
    Truncate,
    /// skip this clipboard change; do not re-prompt until content changes again
    Ignore,
}

/// Sender's decision on a detected clipboard image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardImageDecision {
    /// encode as PNG and ship via the file-transfer channel
    Send,
    /// skip; do not re-prompt until the image changes
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub discoverable_on_startup: bool,
    pub auto_accept_discovered: bool,
    pub start_minimized: bool,
    pub listen_ipv4: Option<IpAddr>,
    pub listen_ipv6: Option<IpAddr>,
    pub ipv6_enabled: bool,
    pub release_bind: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            discoverable_on_startup: false,
            auto_accept_discovered: false,
            start_minimized: false,
            listen_ipv4: None,
            listen_ipv6: None,
            ipv6_enabled: true,
            release_bind: vec![
                "KeyLeftCtrl".into(),
                "KeyLeftShift".into(),
                "KeyLeftMeta".into(),
                "KeyLeftAlt".into(),
            ],
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendRequest {
    /// activate/deactivate client
    Activate(ClientHandle, bool),
    /// add a new client
    Create,
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    Delete(ClientHandle),
    /// request an enumeration of all clients
    Enumerate(),
    /// resolve dns
    ResolveDns(ClientHandle),
    /// update hostname
    UpdateHostname(ClientHandle, Option<String>),
    /// update port
    UpdatePort(ClientHandle, u16),
    /// update position
    UpdatePosition(ClientHandle, Position),
    /// update fix-ips
    UpdateFixIps(ClientHandle, Vec<IpAddr>),
    /// request reenabling input capture
    EnableCapture,
    /// request reenabling input emulation
    EnableEmulation,
    /// synchronize all state
    Sync,
    /// authorize fingerprint (description, fingerprint)
    AuthorizeKey(String, String),
    /// remove fingerprint (fingerprint)
    RemoveAuthorizedKey(String),
    /// change the hook command
    UpdateEnterHook(u64, Option<String>),
    /// save config file
    SaveConfiguration,
    /// accept a discovered device and create a client for it
    AcceptDiscoveredDevice {
        hostname: String,
        addrs: Vec<IpAddr>,
        port: u16,
        fingerprint: String,
        position: Position,
    },
    /// toggle discoverable mode
    SetDiscoverable(bool),
    /// update persistent settings
    UpdateSettings(Settings),
    /// receiver's response to a `FileOfferIncoming`
    RespondFileOffer {
        xfer_id: u64,
        decision: FileDecision,
    },
    /// sender's response to a `ClipboardOverflow` prompt
    RespondClipboardOverflow {
        client: ClientHandle,
        decision: ClipboardOverflowDecision,
    },
    /// sender's response to a `ClipboardImageDetected` prompt
    RespondClipboardImage {
        client: ClientHandle,
        decision: ClipboardImageDecision,
    },
    /// send a file or folder to a specific client. Used by the GTK
    /// drop-widget fallback on compositors without an edge-drop source
    /// (notably GNOME), and by the CLI for manual testing.
    SendFile { client: ClientHandle, path: PathBuf },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Disabled,
    Enabled,
}

impl From<Status> for bool {
    fn from(status: Status) -> Self {
        match status {
            Status::Enabled => true,
            Status::Disabled => false,
        }
    }
}

#[cfg(unix)]
const LAN_MOUSE_SOCKET_NAME: &str = "lan-mouse-socket.sock";

#[derive(Debug, Error)]
pub enum SocketPathError {
    #[error("could not determine $XDG_RUNTIME_DIR: `{0}`")]
    XdgRuntimeDirNotFound(VarError),
    #[error("could not determine $HOME: `{0}`")]
    HomeDirNotFound(VarError),
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let xdg_runtime_dir =
        env::var("XDG_RUNTIME_DIR").map_err(SocketPathError::XdgRuntimeDirNotFound)?;
    Ok(Path::new(xdg_runtime_dir.as_str()).join(LAN_MOUSE_SOCKET_NAME))
}

#[cfg(all(unix, target_os = "macos"))]
pub fn default_socket_path() -> Result<PathBuf, SocketPathError> {
    let home = env::var("HOME").map_err(SocketPathError::HomeDirNotFound)?;
    Ok(Path::new(home.as_str())
        .join("Library")
        .join("Caches")
        .join(LAN_MOUSE_SOCKET_NAME))
}
