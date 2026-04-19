#[cfg(feature = "discovery")]
use crate::discovery::DiscoveryService;
#[cfg(feature = "file_drop")]
use crate::file_transfer::{self, FileTransferService};
use crate::{
    capture::{Capture, CaptureType, ICaptureEvent},
    client::ClientManager,
    clipboard_event::ClipboardChange,
    config::{Config, ConfigClient},
    connect::LanMouseConnection,
    crypto,
    dns::{DnsEvent, DnsResolver},
    emulation::{Emulation, EmulationEvent},
    listen::{LanMouseListener, ListenerCreationError},
};
use futures::StreamExt;
use hickory_resolver::ResolveError;
#[cfg(feature = "file_drop")]
use input_capture::file_drop::{FileDropEvent, FileDropSource, file_drop_source};
use lan_mouse_ipc::{
    AsyncFrontendListener, ClientHandle, FrontendEvent, FrontendRequest, IpcError,
    IpcListenerCreationError, Position, Status,
};
use log;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
};
use thiserror::Error;
use tokio::{process::Command, signal, sync::Notify};

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Dns(#[from] ResolveError),
    #[error(transparent)]
    IpcListen(#[from] IpcListenerCreationError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    ListenError(#[from] ListenerCreationError),
    #[error("failed to load certificate: `{0}`")]
    Certificate(#[from] crypto::Error),
    #[cfg(feature = "file_drop")]
    #[error("file-transfer: {0}")]
    FileTransfer(#[from] crate::file_transfer::FileTransferError),
}

pub struct Service {
    /// configuration
    config: Config,
    /// input capture
    capture: Capture,
    /// input emulation
    emulation: Emulation,
    /// dns resolver
    resolver: DnsResolver,
    /// frontend listener
    frontend_listener: AsyncFrontendListener,
    /// authorized public key sha256 fingerprints
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    /// (outgoing) client information
    client_manager: ClientManager,
    /// current port
    port: u16,
    /// the public key fingerprint for (D)TLS
    public_key_fingerprint: String,
    /// notify for pending frontend events
    frontend_event_pending: Notify,
    /// frontend events queued for sending
    pending_frontend_events: VecDeque<FrontendEvent>,
    /// status of input capture (enabled / disabled)
    capture_status: Status,
    /// status of input emulation (enabled / disabled)
    emulation_status: Status,
    /// keep track of registered connections to avoid duplicate barriers
    incoming_conns: HashSet<SocketAddr>,
    /// map from capture handle to connection info
    incoming_conn_info: HashMap<ClientHandle, Incoming>,
    next_trigger_handle: u64,
    /// clipboard monitor (kept alive)
    #[cfg(feature = "clipboard")]
    clipboard_monitor: Option<crate::clipboard::ClipboardMonitor>,
    /// clipboard change receiver
    clipboard_rx: local_channel::mpsc::Receiver<ClipboardChange>,
    /// Oversize clipboard text awaiting the user's Send/Truncate/Ignore
    /// decision. Latest change wins if the user takes long enough for the
    /// clipboard to change again.
    #[cfg(feature = "clipboard")]
    pending_clipboard_overflow: Option<String>,
    /// PNG-encoded clipboard image awaiting the user's Send/Skip decision.
    #[cfg(feature = "clipboard")]
    pending_clipboard_image: Option<Vec<u8>>,
    /// mDNS discovery event receiver
    discovery_rx: local_channel::mpsc::Receiver<FrontendEvent>,
    /// mDNS discovery service
    #[cfg(feature = "discovery")]
    discovery: Option<DiscoveryService>,
    /// cache of currently discovered devices (hostname -> DiscoveredDevice event).
    /// Used to filter out devices already added as connections, and to
    /// re-surface them if the corresponding connection is later deleted.
    discovered_devices: HashMap<String, FrontendEvent>,
    /// QUIC-based file-transfer side channel
    #[cfg(feature = "file_drop")]
    file_transfer: Option<FileTransferService>,
    /// Incoming file offers awaiting user decision. Keyed by xfer_id.
    /// Retained so a late-attaching GTK frontend can be re-notified via
    /// the Sync path.
    #[cfg(feature = "file_drop")]
    pending_file_offers: HashMap<u64, FrontendEvent>,
    /// Drag-at-edge detector. Dummy backend is a no-op; real backends
    /// (tasks 12, 13) will emit `FileDropEvent::Dropped` when the user
    /// releases a file at a screen edge bordering a connected client.
    #[cfg(feature = "file_drop")]
    file_drop_source: Option<Box<dyn FileDropSource>>,
}

#[derive(Debug)]
struct Incoming {
    fingerprint: String,
    addr: SocketAddr,
    pos: Position,
}

/// Write `content` to a session-scoped temp file under `$XDG_RUNTIME_DIR`
/// (or `/tmp` as a fallback). Used by the clipboard-as-file bridge so the
/// on-disk handoff doesn't leak into the user's real filesystem. Returns
/// the path, or `None` on any I/O error.
#[cfg(all(feature = "clipboard", feature = "file_drop"))]
fn write_clipboard_temp(content: &[u8], ext: &'static str) -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("lan-mouse");
    std::fs::create_dir_all(&base).ok()?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = base.join(format!("clipboard-{ts}.{ext}"));
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// Select-loop arm for the optional file-transfer service. Returns a future
/// that either yields the next event or, if the service is not running,
/// stays pending forever.
#[cfg(feature = "file_drop")]
async fn next_file_transfer_event(
    ft: &mut Option<FileTransferService>,
) -> Option<file_transfer::Event> {
    match ft {
        Some(svc) => svc.next_event().await,
        None => std::future::pending().await,
    }
}

/// Select-loop arm for the optional file-drop source. The dummy backend
/// returns `Poll::Pending` forever; real backends emit `FileDropEvent`s when
/// the user drags a file toward an active edge.
#[cfg(feature = "file_drop")]
async fn next_file_drop_event(src: &mut Option<Box<dyn FileDropSource>>) -> Option<FileDropEvent> {
    match src {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

impl Service {
    pub async fn new(config: Config) -> Result<Self, ServiceError> {
        let client_manager = ClientManager::default();
        for client in config.clients() {
            client_manager.add_with_config(client);
        }

        // load certificate
        let cert = crypto::load_or_generate_key_and_cert(config.cert_path())?;
        let public_key_fingerprint = crypto::certificate_fingerprint(&cert);

        // create frontend communication adapter, exit if already running
        let frontend_listener = AsyncFrontendListener::new().await?;

        let authorized_keys = Arc::new(RwLock::new(config.authorized_fingerprints()));

        // Log listening interfaces
        let ipv6_enabled = config.ipv6_enabled();
        let listen_ipv4 = config.listen_ipv4();
        let listen_ipv6 = config.listen_ipv6();
        if ipv6_enabled {
            log::info!(
                "listening on IPv4: {}, IPv6: {}, port: {}",
                listen_ipv4
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "all (0.0.0.0)".into()),
                listen_ipv6
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "all ([::])".into()),
                config.port()
            );
        } else {
            log::info!(
                "listening on IPv4: {}, IPv6: disabled, port: {}",
                listen_ipv4
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "all (0.0.0.0)".into()),
                config.port()
            );
        }

        // listener + connection
        let listener = LanMouseListener::new(
            config.port(),
            cert.clone(),
            authorized_keys.clone(),
            ipv6_enabled,
        )
        .await?;
        let conn = LanMouseConnection::new(cert.clone(), client_manager.clone(), ipv6_enabled);

        // input capture + emulation
        let capture_backend = config.capture_backend().map(|b| b.into());
        let capture = Capture::new(capture_backend, conn, config.release_bind());
        let emulation_backend = config.emulation_backend().map(|b| b.into());
        let emulation = Emulation::new(emulation_backend, listener);

        // create dns resolver
        let resolver = DnsResolver::new()?;

        let port = config.port();

        // initialize clipboard monitor
        #[cfg(feature = "clipboard")]
        let (clipboard_monitor, clipboard_rx) = {
            match crate::clipboard::ClipboardMonitor::new() {
                Ok((m, rx)) => {
                    log::info!("clipboard sharing enabled");
                    (Some(m), rx)
                }
                Err(e) => {
                    log::warn!("clipboard sharing unavailable: {e}");
                    let (_, rx) = local_channel::mpsc::channel::<ClipboardChange>();
                    (None, rx)
                }
            }
        };
        #[cfg(not(feature = "clipboard"))]
        let clipboard_rx = {
            let (_, rx) = local_channel::mpsc::channel::<ClipboardChange>();
            rx
        };

        // initialize mDNS discovery
        #[cfg(feature = "discovery")]
        let (discovery, discovery_rx) = {
            let hostname = hostname::get()
                .ok()
                .and_then(|h| h.to_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "lan-mouse".to_string());
            let discoverable = config.discoverable();
            match DiscoveryService::new(
                port,
                public_key_fingerprint.clone(),
                hostname,
                discoverable,
            ) {
                Ok((d, rx)) => {
                    log::info!("mDNS discovery enabled (discoverable: {discoverable})");
                    (Some(d), rx)
                }
                Err(e) => {
                    log::warn!("mDNS discovery unavailable: {e}");
                    let (_, rx) = local_channel::mpsc::channel();
                    (None, rx)
                }
            }
        };
        #[cfg(not(feature = "discovery"))]
        let discovery_rx = {
            let (_, rx) = local_channel::mpsc::channel::<FrontendEvent>();
            rx
        };

        // initialize file-transfer side channel
        #[cfg(feature = "file_drop")]
        let file_transfer = {
            let limits = file_transfer::Limits {
                max_bytes: config.max_file_transfer_bytes(),
                max_entries: config.max_file_transfer_entries(),
            };
            match FileTransferService::start(
                config.file_transfer_port(),
                ipv6_enabled,
                config.cert_path(),
                authorized_keys.clone(),
                limits,
            )
            .await
            {
                Ok(svc) => {
                    log::info!(
                        "file-transfer service ready on port {}",
                        config.file_transfer_port()
                    );
                    Some(svc)
                }
                Err(e) => {
                    log::warn!("file-transfer service unavailable: {e}");
                    None
                }
            }
        };

        let service = Self {
            config,
            capture,
            emulation,
            frontend_listener,
            resolver,
            authorized_keys,
            public_key_fingerprint,
            client_manager,
            frontend_event_pending: Default::default(),
            port,
            pending_frontend_events: Default::default(),
            capture_status: Default::default(),
            emulation_status: Default::default(),
            incoming_conn_info: Default::default(),
            incoming_conns: Default::default(),
            next_trigger_handle: 0,
            #[cfg(feature = "clipboard")]
            clipboard_monitor,
            clipboard_rx,
            #[cfg(feature = "clipboard")]
            pending_clipboard_overflow: None,
            #[cfg(feature = "clipboard")]
            pending_clipboard_image: None,
            discovery_rx,
            #[cfg(feature = "discovery")]
            discovery,
            discovered_devices: HashMap::new(),
            #[cfg(feature = "file_drop")]
            file_transfer,
            #[cfg(feature = "file_drop")]
            pending_file_offers: HashMap::new(),
            #[cfg(feature = "file_drop")]
            file_drop_source: Some(file_drop_source(None)),
        };
        Ok(service)
    }

    pub async fn run(&mut self) -> Result<(), ServiceError> {
        let active = self.client_manager.active_clients();
        for handle in active.iter() {
            // small hack: `activate_client()` checks, if the client
            // is already active in client_manager and does not create a
            // capture barrier in that case so we have to deactivate it first
            self.client_manager.deactivate_client(*handle);
        }

        for handle in active {
            self.activate_client(handle);
        }

        // tokio::select! doesn't support `#[cfg(...)]` on arms, so two
        // near-identical blocks live here — one with the file-drop arms,
        // one without. Keep them in sync when adding new arms.
        let exit_signal: Result<(), io::Error> = loop {
            #[cfg(feature = "file_drop")]
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                Some(change) = self.clipboard_rx.next() => self.handle_clipboard_changed(change),
                Some(event) = self.discovery_rx.next() => self.handle_discovery_event(event),
                Some(event) = next_file_transfer_event(&mut self.file_transfer) => self.handle_file_transfer_event(event),
                Some(event) = next_file_drop_event(&mut self.file_drop_source) => self.handle_file_drop_event(event),
                r = signal::ctrl_c() => break r,
            }
            #[cfg(not(feature = "file_drop"))]
            tokio::select! {
                request = self.frontend_listener.next() => self.handle_frontend_request(request),
                _ = self.frontend_event_pending.notified() => self.handle_frontend_pending().await,
                event = self.emulation.event() => self.handle_emulation_event(event),
                event = self.capture.event() => self.handle_capture_event(event),
                event = self.resolver.event() => self.handle_resolver_event(event),
                _ = self.config.changed() => self.handle_config_change(),
                Some(change) = self.clipboard_rx.next() => self.handle_clipboard_changed(change),
                Some(event) = self.discovery_rx.next() => self.handle_discovery_event(event),
                r = signal::ctrl_c() => break r,
            }
        };
        exit_signal.expect("failed to wait for CTRL+C");

        log::info!("terminating service ...");
        log::debug!("terminating capture ...");
        self.capture.terminate().await;
        log::debug!("terminating emulation ...");
        self.emulation.terminate().await;
        log::debug!("terminating dns resolver ...");
        self.resolver.terminate().await;

        Ok(())
    }

    fn handle_frontend_request(&mut self, request: Option<Result<FrontendRequest, IpcError>>) {
        let request = match request.expect("frontend listener closed") {
            Ok(r) => r,
            Err(e) => return log::error!("error receiving request: {e}"),
        };
        match request {
            FrontendRequest::Activate(handle, active) => {
                self.set_client_active(handle, active);
                self.save_config();
            }
            FrontendRequest::AuthorizeKey(desc, fp) => {
                self.add_authorized_key(desc, fp);
                self.save_config();
            }
            FrontendRequest::ChangePort(port) => self.change_port(port),
            FrontendRequest::Create => {
                self.add_client();
                self.save_config();
            }
            FrontendRequest::Delete(handle) => {
                self.remove_client(handle);
                self.save_config();
            }
            FrontendRequest::EnableCapture => self.capture.reenable(),
            FrontendRequest::EnableEmulation => self.emulation.reenable(),
            FrontendRequest::Enumerate() => self.enumerate(),
            FrontendRequest::UpdateFixIps(handle, fix_ips) => {
                self.update_fix_ips(handle, fix_ips);
                self.save_config();
            }
            FrontendRequest::UpdateHostname(handle, host) => {
                self.update_hostname(handle, host);
                self.save_config();
            }
            FrontendRequest::UpdatePort(handle, port) => {
                self.update_port(handle, port);
                self.save_config();
            }
            FrontendRequest::UpdatePosition(handle, pos) => {
                self.update_pos(handle, pos);
                self.save_config();
            }
            FrontendRequest::ResolveDns(handle) => self.resolve(handle),
            FrontendRequest::Sync => self.sync_frontend(),
            FrontendRequest::RemoveAuthorizedKey(key) => {
                self.remove_authorized_key(key);
                self.save_config();
            }
            FrontendRequest::UpdateEnterHook(handle, enter_hook) => {
                self.update_enter_hook(handle, enter_hook)
            }
            FrontendRequest::SaveConfiguration => self.save_config(),
            FrontendRequest::AcceptDiscoveredDevice {
                hostname,
                addrs,
                port,
                fingerprint,
                position,
            } => {
                self.accept_discovered_device(hostname, addrs, port, fingerprint, position);
            }
            FrontendRequest::SetDiscoverable(discoverable) => {
                self.set_discoverable(discoverable);
            }
            FrontendRequest::UpdateSettings(settings) => {
                self.update_settings(settings);
            }
            FrontendRequest::RespondFileOffer { xfer_id, decision } => {
                self.handle_respond_file_offer(xfer_id, decision);
            }
            FrontendRequest::RespondClipboardOverflow { decision, .. } => {
                #[cfg(feature = "clipboard")]
                self.handle_respond_clipboard_overflow(decision);
                #[cfg(not(feature = "clipboard"))]
                let _ = decision;
            }
            FrontendRequest::RespondClipboardImage { decision, .. } => {
                #[cfg(feature = "clipboard")]
                self.handle_respond_clipboard_image(decision);
                #[cfg(not(feature = "clipboard"))]
                let _ = decision;
            }
        }
    }

    fn save_config(&mut self) {
        let clients = self.client_manager.clients();
        let clients = clients
            .into_iter()
            // Skip empty placeholder connections (no hostname and no fixed IPs)
            // so closing the app with an unfilled "+ Add" row doesn't persist.
            .filter(|(c, _)| {
                let has_hostname = c
                    .hostname
                    .as_deref()
                    .map(|h| !h.is_empty())
                    .unwrap_or(false);
                has_hostname || !c.fix_ips.is_empty()
            })
            .map(|(c, s)| ConfigClient {
                ips: HashSet::from_iter(c.fix_ips),
                hostname: c.hostname,
                port: c.port,
                pos: c.pos,
                active: s.active,
                enter_hook: c.cmd,
            })
            .collect();
        self.config.set_clients(clients);
        let authorized_keys = self.authorized_keys.read().expect("lock").clone();
        self.config.set_authorized_keys(authorized_keys);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
    }

    fn handle_config_change(&mut self) {
        for h in self.client_manager.registered_clients() {
            self.remove_client(h);
        }
        for c in self.config.clients() {
            let handle = self.client_manager.add_with_config(c);
            log::info!("added client {handle}");
            let (c, s) = self.client_manager.get_state(handle).unwrap();
            if s.active {
                self.client_manager.deactivate_client(handle);
                self.activate_client(handle);
            }
            self.notify_frontend(FrontendEvent::Created(handle, c, s));
        }
        let release_bind = self.config.release_bind();
        self.capture.set_release_bind(release_bind);
        let authorized_keys = self.config.authorized_fingerprints();
        self.authorized_keys
            .write()
            .unwrap()
            .clone_from(&authorized_keys);
        self.sync_frontend();
    }

    async fn handle_frontend_pending(&mut self) {
        while let Some(event) = self.pending_frontend_events.pop_front() {
            self.frontend_listener.broadcast(event).await;
        }
    }

    fn handle_emulation_event(&mut self, event: EmulationEvent) {
        match event {
            EmulationEvent::ConnectionAttempt { fingerprint } => {
                // If discoverable is on, auto-authorize incoming connection attempts
                #[cfg(feature = "discovery")]
                if self.discovery.as_ref().is_some_and(|d| d.is_discoverable()) {
                    log::info!(
                        "auto-authorizing incoming connection: {}",
                        &fingerprint[..16]
                    );
                    self.add_authorized_key("discovered-peer".into(), fingerprint.clone());
                    self.save_config();
                }
                self.notify_frontend(FrontendEvent::ConnectionAttempt { fingerprint });
            }
            EmulationEvent::Entered {
                addr,
                pos,
                fingerprint,
            } => {
                // check if already registered
                if !self.incoming_conns.contains(&addr) {
                    self.add_incoming(addr, pos, fingerprint.clone());
                    self.notify_frontend(FrontendEvent::DeviceEntered {
                        fingerprint,
                        addr,
                        pos,
                    });
                    // NOTE: no clipboard push from the emulation (receiving) side on enter.
                    // The capturing side (the one moving the cursor) is treated as
                    // authoritative and pushes its clipboard via `ICaptureEvent::ClientEntered`,
                    // so this side only receives. Real-time copies on this device still
                    // propagate via the polling broadcast path.
                } else {
                    self.update_incoming(addr, pos, fingerprint);
                }
            }
            EmulationEvent::Disconnected { addr } => {
                if let Some(addr) = self.remove_incoming(addr) {
                    self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
                }
            }
            EmulationEvent::PortChanged(port) => match port {
                Ok(port) => {
                    self.port = port;
                    self.notify_frontend(FrontendEvent::PortChanged(port, None));
                }
                Err(e) => self
                    .notify_frontend(FrontendEvent::PortChanged(self.port, Some(format!("{e}")))),
            },
            EmulationEvent::EmulationDisabled => {
                self.emulation_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::EmulationEnabled => {
                self.emulation_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
            }
            EmulationEvent::ReleaseNotify => self.capture.release(),
            EmulationEvent::Connected { addr, fingerprint } => {
                self.notify_frontend(FrontendEvent::DeviceConnected { addr, fingerprint });
            }
            #[cfg(feature = "clipboard")]
            EmulationEvent::ClipboardReceived(text) => {
                log::info!("clipboard received ({} bytes)", text.len());
                if let Some(ref clipboard) = self.clipboard_monitor {
                    clipboard.set_text(&text);
                }
            }
        }
    }

    fn handle_capture_event(&mut self, event: ICaptureEvent) {
        match event {
            ICaptureEvent::CaptureBegin(handle) => {
                // we entered the capture zone for an incoming connection
                // => notify it that its capture should be released
                if let Some(incoming) = self.incoming_conn_info.get(&handle) {
                    self.emulation.send_leave_event(incoming.addr);
                }
            }
            ICaptureEvent::CaptureDisabled => {
                self.capture_status = Status::Disabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::CaptureEnabled => {
                self.capture_status = Status::Enabled;
                self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
            }
            ICaptureEvent::ClientEntered(handle) => {
                log::info!("entering client {handle} ...");
                self.spawn_hook_command(handle);
                // Sync clipboard to the client we're entering
                #[cfg(feature = "clipboard")]
                if let Some(ref monitor) = self.clipboard_monitor {
                    let clip = monitor.get_current_text();
                    if !clip.is_empty() {
                        log::info!(
                            "syncing clipboard to client on enter ({} bytes)",
                            clip.len()
                        );
                        let data = crate::clipboard::encode_clipboard_msg(&clip);
                        self.capture.send_clipboard(&data);
                    }
                }
            }
            #[cfg(feature = "clipboard")]
            ICaptureEvent::ClipboardReceived(text) => {
                log::info!(
                    "clipboard received from remote client ({} bytes)",
                    text.len()
                );
                if let Some(ref clipboard) = self.clipboard_monitor {
                    clipboard.set_text(&text);
                }
            }
        }
    }

    fn handle_resolver_event(&mut self, event: DnsEvent) {
        let handle = match event {
            DnsEvent::Resolving(handle) => {
                self.client_manager.set_resolving(handle, true);
                handle
            }
            DnsEvent::Resolved(handle, hostname, ips) => {
                self.client_manager.set_resolving(handle, false);
                if let Err(e) = &ips {
                    log::warn!("could not resolve {hostname}: {e}");
                }
                let ips = ips.unwrap_or_default();
                self.client_manager.set_dns_ips(handle, ips);
                handle
            }
        };
        self.broadcast_client(handle);
    }

    fn resolve(&self, handle: ClientHandle) {
        if let Some(hostname) = self.client_manager.get_hostname(handle) {
            self.resolver.resolve(handle, hostname);
        }
    }

    fn sync_frontend(&mut self) {
        self.enumerate();
        self.notify_frontend(FrontendEvent::EmulationStatus(self.emulation_status));
        self.notify_frontend(FrontendEvent::CaptureStatus(self.capture_status));
        self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        self.notify_frontend(FrontendEvent::PublicKeyFingerprint(
            self.public_key_fingerprint.clone(),
        ));
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
        let settings = self.config.settings();
        self.notify_frontend(FrontendEvent::SettingsChanged(settings));
        let discoverable = self.config.discoverable();
        self.notify_frontend(FrontendEvent::DiscoverableChanged(discoverable));
        // Re-emit any file-transfer offers that arrived before the frontend
        // attached. Without this, offers queued while running headless are
        // silently lost when the user finally opens the GTK UI.
        #[cfg(feature = "file_drop")]
        {
            let replay: Vec<FrontendEvent> = self.pending_file_offers.values().cloned().collect();
            for ev in replay {
                self.notify_frontend(ev);
            }
        }
    }

    const ENTER_HANDLE_BEGIN: u64 = u64::MAX / 2 + 1;

    fn add_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let handle = Self::ENTER_HANDLE_BEGIN + self.next_trigger_handle;
        self.next_trigger_handle += 1;
        self.capture.create(handle, pos, CaptureType::EnterOnly);
        self.incoming_conns.insert(addr);
        self.incoming_conn_info.insert(
            handle,
            Incoming {
                fingerprint,
                addr,
                pos,
            },
        );
    }

    fn update_incoming(&mut self, addr: SocketAddr, pos: Position, fingerprint: String) {
        let incoming = self
            .incoming_conn_info
            .iter_mut()
            .find(|(_, i)| i.addr == addr)
            .map(|(_, i)| i)
            .expect("no such client");
        let mut changed = false;
        if incoming.fingerprint != fingerprint {
            incoming.fingerprint = fingerprint.clone();
            changed = true;
        }
        if incoming.pos != pos {
            incoming.pos = pos;
            changed = true;
        }
        if changed {
            self.remove_incoming(addr);
            self.add_incoming(addr, pos, fingerprint.clone());
            self.notify_frontend(FrontendEvent::IncomingDisconnected(addr));
            self.notify_frontend(FrontendEvent::DeviceEntered {
                fingerprint,
                addr,
                pos,
            });
        }
    }

    fn remove_incoming(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let handle = self
            .incoming_conn_info
            .iter()
            .find(|(_, incoming)| incoming.addr == addr)
            .map(|(k, _)| *k)?;
        self.capture.destroy(handle);
        self.incoming_conns.remove(&addr);
        self.incoming_conn_info
            .remove(&handle)
            .map(|incoming| incoming.addr)
    }

    fn notify_frontend(&mut self, event: FrontendEvent) {
        self.pending_frontend_events.push_back(event);
        self.frontend_event_pending.notify_one();
    }

    /// Forward an event from the QUIC file-transfer service to the GTK frontend.
    /// Incoming offers are also queued in `pending_file_offers` so a late-
    /// attaching frontend can pick them up via the `Sync` path.
    #[cfg(feature = "file_drop")]
    fn handle_file_transfer_event(&mut self, event: file_transfer::Event) {
        use lan_mouse_ipc::TransferResult as IpcResult;
        match event {
            file_transfer::Event::IncomingOffer {
                xfer_id,
                peer_addr,
                peer_fingerprint,
                root_name,
                entries,
                total_bytes,
            } => {
                let client = self.lookup_client_by_addr(peer_addr).unwrap_or(0);
                let ev = FrontendEvent::FileOfferIncoming {
                    xfer_id,
                    client,
                    root_name,
                    entries,
                    total_bytes,
                    fingerprint: peer_fingerprint,
                };
                self.pending_file_offers.insert(xfer_id, ev.clone());
                self.notify_frontend(ev);
            }
            file_transfer::Event::Progress {
                xfer_id,
                bytes,
                total,
                entries_done,
                entries_total,
                current_entry,
            } => {
                self.notify_frontend(FrontendEvent::FileTransferProgress {
                    xfer_id,
                    bytes,
                    total,
                    entries_done,
                    entries_total,
                    current_entry,
                });
            }
            file_transfer::Event::Finished { xfer_id, result } => {
                self.pending_file_offers.remove(&xfer_id);
                let ipc = match result {
                    file_transfer::TransferResult::Ok { destination } => {
                        IpcResult::Ok { destination }
                    }
                    file_transfer::TransferResult::Cancelled => IpcResult::Cancelled,
                    file_transfer::TransferResult::Error(e) => IpcResult::Error(e),
                };
                self.notify_frontend(FrontendEvent::FileTransferFinished {
                    xfer_id,
                    result: ipc,
                });
            }
        }
    }

    /// Dispatch a user's Accept/Decline decision back into the file-transfer
    /// service. Called from `handle_frontend_request` on `RespondFileOffer`.
    #[cfg(feature = "file_drop")]
    fn handle_respond_file_offer(&mut self, xfer_id: u64, decision: lan_mouse_ipc::FileDecision) {
        self.pending_file_offers.remove(&xfer_id);
        let Some(ft) = &self.file_transfer else {
            log::warn!("RespondFileOffer without running file-transfer service");
            return;
        };
        let decision = match decision {
            lan_mouse_ipc::FileDecision::Accept { dest_dir } => {
                file_transfer::Decision::Accept { dest_dir }
            }
            lan_mouse_ipc::FileDecision::Decline => file_transfer::Decision::Decline,
        };
        ft.try_send_command(file_transfer::Command::RespondOffer { xfer_id, decision });
    }

    /// Best-effort lookup of a configured client handle by its peer address.
    /// Falls back to `None` if the peer is authorized-by-fingerprint but not
    /// a configured client (perfectly valid; the GTK UI will fall back to the
    /// fingerprint for display).
    #[cfg(feature = "file_drop")]
    fn lookup_client_by_addr(&self, addr: SocketAddr) -> Option<ClientHandle> {
        for (h, incoming) in &self.incoming_conn_info {
            if incoming.addr.ip() == addr.ip() {
                return Some(*h);
            }
        }
        None
    }

    /// Route a file-drop event to the outgoing transfer path.
    ///
    /// `Entered` / `Cancelled` are advisory and are just logged for now; the
    /// real work is `Dropped`, where we resolve the target edge → active
    /// client → (address, fingerprint) and kick off one `SendPath` command
    /// per dropped path.
    #[cfg(feature = "file_drop")]
    fn handle_file_drop_event(&mut self, event: FileDropEvent) {
        let (position, paths) = match event {
            FileDropEvent::Entered(p) => {
                log::debug!("drag entered {p}");
                return;
            }
            FileDropEvent::Cancelled(p) => {
                log::debug!("drag cancelled at {p}");
                return;
            }
            FileDropEvent::Dropped { position, paths } => (position, paths),
        };
        if paths.is_empty() {
            return;
        }
        let Some(ft) = &self.file_transfer else {
            log::warn!("file drop at {position}: file-transfer service unavailable");
            return;
        };

        let Some((target_addr, target_fp)) = self.resolve_drop_target(position) else {
            log::warn!(
                "file drop at {position}: no connected client at that edge (peer must have previously connected for the fingerprint to be known)"
            );
            return;
        };

        for path in paths {
            log::info!("dispatching file drop {path:?} → {target_addr}");
            ft.try_send_command(file_transfer::Command::SendPath {
                target_addr,
                target_fingerprint: target_fp.clone(),
                root: path,
            });
        }
    }

    /// Given an input-capture edge position, resolve the active client at
    /// that edge and return the `(target_addr, fingerprint)` pair needed by
    /// [`file_transfer::Command::SendPath`]. Returns `None` if no active
    /// client exists at the edge or its fingerprint is unknown.
    ///
    /// Fingerprint is learned from `incoming_conn_info` — i.e., the peer
    /// must have previously established a DTLS session with us at least
    /// once. That matches how the existing mouse-sharing trust works.
    #[cfg(feature = "file_drop")]
    fn resolve_drop_target(&self, edge: input_capture::Position) -> Option<(SocketAddr, String)> {
        let ipc_pos = match edge {
            input_capture::Position::Left => Position::Left,
            input_capture::Position::Right => Position::Right,
            input_capture::Position::Top => Position::Top,
            input_capture::Position::Bottom => Position::Bottom,
        };
        // Find an active client configured at this position with a known
        // recent peer address.
        let mut candidate: Option<SocketAddr> = None;
        for (_, cfg, state) in self.client_manager.get_client_states() {
            if cfg.pos != ipc_pos || !state.active {
                continue;
            }
            if let Some(addr) = state.active_addr {
                candidate = Some(addr);
                break;
            }
            // Fall back to the first known IP + configured port.
            if let Some(ip) = state.ips.iter().next() {
                candidate = Some(SocketAddr::new(*ip, cfg.port));
            }
        }
        let target_addr = candidate?;

        let fingerprint = self
            .incoming_conn_info
            .values()
            .find(|i| i.addr.ip() == target_addr.ip())
            .map(|i| i.fingerprint.clone())?;

        // Rewrite the target port from the DTLS port to the QUIC port.
        // We land on the QUIC endpoint for bulk transfer, not DTLS.
        let quic_addr = SocketAddr::new(target_addr.ip(), self.config.file_transfer_port());
        Some((quic_addr, fingerprint))
    }

    fn add_authorized_key(&mut self, desc: String, fp: String) {
        self.authorized_keys.write().expect("lock").insert(fp, desc);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn remove_authorized_key(&mut self, fp: String) {
        self.authorized_keys.write().expect("lock").remove(&fp);
        let keys = self.authorized_keys.read().expect("lock").clone();
        self.notify_frontend(FrontendEvent::AuthorizedUpdated(keys));
    }

    fn enumerate(&mut self) {
        let clients = self.client_manager.get_client_states();
        self.notify_frontend(FrontendEvent::Enumerate(clients));
    }

    fn add_client(&mut self) {
        let handle = self.client_manager.add_client();
        log::info!("added client {handle}");
        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));
    }

    fn set_client_active(&mut self, handle: ClientHandle, active: bool) {
        if active {
            self.activate_client(handle);
        } else {
            self.deactivate_client(handle);
        }
    }

    fn deactivate_client(&mut self, handle: ClientHandle) {
        log::debug!("deactivating client {handle}");
        if self.client_manager.deactivate_client(handle) {
            self.capture.destroy(handle);
            self.broadcast_client(handle);
            log::info!("deactivated client {handle}");
        }
    }

    fn activate_client(&mut self, handle: ClientHandle) {
        log::debug!("activating client {handle}");

        /* resolve dns on activate */
        self.resolve(handle);

        /* deactivate potential other client at this position */
        let Some(pos) = self.client_manager.get_pos(handle) else {
            return;
        };

        if let Some(other) = self.client_manager.client_at(pos) {
            if other != handle {
                self.deactivate_client(other);
            }
        }

        /* activate the client */
        if self.client_manager.activate_client(handle) {
            /* notify capture and frontends */
            self.capture.create(handle, pos, CaptureType::Default);
            self.broadcast_client(handle);
            log::info!("activated client {handle} ({pos})");
        }
    }

    fn change_port(&mut self, port: u16) {
        if self.port != port {
            self.emulation.request_port_change(port);
        } else {
            self.notify_frontend(FrontendEvent::PortChanged(self.port, None));
        }
    }

    fn remove_client(&mut self, handle: ClientHandle) {
        let removed = self.client_manager.remove_client(handle);
        if let Some((_, ref s)) = removed {
            if s.active {
                self.capture.destroy(handle);
            }
        }
        self.notify_frontend(FrontendEvent::Deleted(handle));

        // If the removed connection's hostname still matches a cached
        // discovered device (and no *other* connection uses it), re-surface it.
        if let Some((c, _)) = removed {
            if let Some(hostname) = c.hostname {
                if !self.hostname_has_client(&hostname) {
                    if let Some(event) = self.discovered_devices.get(&hostname).cloned() {
                        log::info!("re-surfacing discovered device {hostname} after delete");
                        self.notify_frontend(event);
                    }
                }
            }
        }
    }

    fn update_fix_ips(&mut self, handle: ClientHandle, fix_ips: Vec<IpAddr>) {
        self.client_manager.set_fix_ips(handle, fix_ips);
        self.broadcast_client(handle);
    }

    fn update_hostname(&mut self, handle: ClientHandle, hostname: Option<String>) {
        log::info!("hostname changed: {hostname:?}");
        // Detect if the input is already an IP address
        if let Some(ref h) = hostname {
            if let Ok(ip) = h.parse::<IpAddr>() {
                log::info!("detected IP address input: {ip}, using directly");
                self.client_manager.set_hostname(handle, hostname);
                self.client_manager.set_fix_ips(handle, vec![ip]);
                self.broadcast_client(handle);
                return;
            }
        }
        if self.client_manager.set_hostname(handle, hostname) {
            self.resolve(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_port(&mut self, handle: ClientHandle, port: u16) {
        self.client_manager.set_port(handle, port);
        self.broadcast_client(handle);
    }

    fn update_pos(&mut self, handle: ClientHandle, pos: Position) {
        // update state in event input emulator & input capture
        if self.client_manager.set_pos(handle, pos) {
            self.deactivate_client(handle);
            self.activate_client(handle);
        }
        self.broadcast_client(handle);
    }

    fn update_enter_hook(&mut self, handle: ClientHandle, enter_hook: Option<String>) {
        self.client_manager.set_enter_hook(handle, enter_hook);
        self.broadcast_client(handle);
    }

    fn broadcast_client(&mut self, handle: ClientHandle) {
        let event = self
            .client_manager
            .get_state(handle)
            .map(|(c, s)| FrontendEvent::State(handle, c, s))
            .unwrap_or(FrontendEvent::NoSuchClient(handle));
        self.notify_frontend(event);
    }

    fn handle_clipboard_changed(&mut self, change: ClipboardChange) {
        match change {
            ClipboardChange::Text(text) => {
                log::info!("clipboard changed, broadcasting ({} bytes)", text.len());
                #[cfg(feature = "clipboard")]
                {
                    let data = crate::clipboard::encode_clipboard_msg(&text);
                    self.emulation.send_clipboard(&data);
                    self.capture.send_clipboard(&data);
                }
                let _ = text;
            }
            ClipboardChange::OversizeText { content } => {
                self.handle_clipboard_oversize(content);
            }
            ClipboardChange::ImagePng { png, width, height } => {
                self.handle_clipboard_image(png, width, height);
            }
        }
    }

    /// Stash the oversize text and prompt the sender-side frontend.
    #[cfg(feature = "clipboard")]
    fn handle_clipboard_oversize(&mut self, content: String) {
        let bytes = content.len() as u64;
        let preview = content.chars().take(120).collect::<String>();
        let target_client = self.active_transfer_target_handle().unwrap_or(0);
        self.pending_clipboard_overflow = Some(content);
        self.notify_frontend(FrontendEvent::ClipboardOverflow {
            client: target_client,
            bytes,
            preview,
        });
    }

    /// Fallback path when the `clipboard` feature is disabled — we never
    /// get `OversizeText` events, so this is effectively dead, but it keeps
    /// `handle_clipboard_changed` uniformly callable.
    #[cfg(not(feature = "clipboard"))]
    fn handle_clipboard_oversize(&mut self, _content: String) {}

    #[cfg(feature = "clipboard")]
    fn handle_clipboard_image(&mut self, png: Vec<u8>, width: u32, height: u32) {
        let bytes = png.len() as u64;
        let target_client = self.active_transfer_target_handle().unwrap_or(0);
        self.pending_clipboard_image = Some(png);
        self.notify_frontend(FrontendEvent::ClipboardImageDetected {
            client: target_client,
            bytes,
            width,
            height,
        });
    }

    #[cfg(not(feature = "clipboard"))]
    fn handle_clipboard_image(&mut self, _png: Vec<u8>, _width: u32, _height: u32) {}

    /// Handle the sender-side response to a `ClipboardOverflow` prompt.
    #[cfg(feature = "clipboard")]
    fn handle_respond_clipboard_overflow(
        &mut self,
        decision: lan_mouse_ipc::ClipboardOverflowDecision,
    ) {
        use lan_mouse_ipc::ClipboardOverflowDecision as D;
        let Some(content) = self.pending_clipboard_overflow.take() else {
            log::debug!("RespondClipboardOverflow with no pending content (already acted on?)");
            return;
        };
        match decision {
            D::Send => {
                self.send_clipboard_as_file(&content.into_bytes(), "txt");
            }
            D::Truncate => {
                // Legacy behaviour: truncate at the inline cap and sync.
                let truncated = {
                    let s = &content[..];
                    let max = crate::clipboard::MAX_CLIPBOARD_SIZE.min(s.len());
                    // Keep the truncation on a char boundary so we don't
                    // mid-split a UTF-8 sequence.
                    let mut cut = max;
                    while cut > 0 && !s.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    s[..cut].to_string()
                };
                let data = crate::clipboard::encode_clipboard_msg(&truncated);
                self.emulation.send_clipboard(&data);
                self.capture.send_clipboard(&data);
                if let Some(m) = &self.clipboard_monitor {
                    m.note_text(&truncated);
                }
            }
            D::Ignore => {
                log::debug!("clipboard overflow ignored by user");
            }
        }
    }

    #[cfg(feature = "clipboard")]
    fn handle_respond_clipboard_image(&mut self, decision: lan_mouse_ipc::ClipboardImageDecision) {
        use lan_mouse_ipc::ClipboardImageDecision as D;
        let Some(png) = self.pending_clipboard_image.take() else {
            log::debug!("RespondClipboardImage with no pending content");
            return;
        };
        match decision {
            D::Send => self.send_clipboard_as_file(&png, "png"),
            D::Skip => log::debug!("clipboard image skipped by user"),
        }
    }

    /// Write the given bytes to a session-scoped temp file and dispatch a
    /// file-transfer SendPath targeting the currently-active client. The
    /// temp file lives in `$XDG_RUNTIME_DIR/lan-mouse/` (tmpfs, auto-purged
    /// on logout). Dead code without the `file_drop` feature since there's
    /// no QUIC channel to route through.
    #[cfg(all(feature = "clipboard", feature = "file_drop"))]
    fn send_clipboard_as_file(&self, content: &[u8], ext: &'static str) {
        let Some(ft) = &self.file_transfer else {
            log::warn!("clipboard-as-file dropped: file-transfer service unavailable");
            return;
        };
        let Some((target_addr, fingerprint)) = self.active_transfer_target() else {
            log::warn!("clipboard-as-file dropped: no active client with a known fingerprint");
            return;
        };
        let Some(path) = write_clipboard_temp(content, ext) else {
            log::warn!("failed to write clipboard temp file");
            return;
        };
        log::info!(
            "routing clipboard ({} bytes) → {target_addr} via temp file {path:?}",
            content.len()
        );
        ft.try_send_command(file_transfer::Command::SendPath {
            target_addr,
            target_fingerprint: fingerprint,
            root: path,
        });
    }

    #[cfg(all(feature = "clipboard", not(feature = "file_drop")))]
    fn send_clipboard_as_file(&self, _content: &[u8], _ext: &'static str) {
        log::warn!("clipboard-as-file requires the file_drop feature; rebuild with it enabled");
    }

    /// Find an active client and the `(QUIC addr, fingerprint)` pair needed
    /// to initiate a transfer to it. Returns `None` if no client is active
    /// or its fingerprint hasn't been observed from a previous incoming
    /// connection.
    #[cfg(all(feature = "clipboard", feature = "file_drop"))]
    fn active_transfer_target(&self) -> Option<(SocketAddr, String)> {
        for (_, _, state) in self.client_manager.get_client_states() {
            if !state.active {
                continue;
            }
            let addr = state
                .active_addr
                .or_else(|| state.ips.iter().next().map(|ip| SocketAddr::new(*ip, 0)))?;
            let fingerprint = self
                .incoming_conn_info
                .values()
                .find(|i| i.addr.ip() == addr.ip())
                .map(|i| i.fingerprint.clone())?;
            let quic_addr = SocketAddr::new(addr.ip(), self.config.file_transfer_port());
            return Some((quic_addr, fingerprint));
        }
        None
    }

    /// Pure-lookup variant that returns the active client's handle — used
    /// when we only want to populate the `client:` field of an IPC event
    /// without having to resolve the fingerprint.
    #[cfg(feature = "clipboard")]
    fn active_transfer_target_handle(&self) -> Option<ClientHandle> {
        for (h, _, state) in self.client_manager.get_client_states() {
            if state.active {
                return Some(h);
            }
        }
        None
    }

    fn set_discoverable(&mut self, discoverable: bool) {
        #[cfg(feature = "discovery")]
        if let Some(ref mut discovery) = self.discovery {
            if let Err(e) = discovery.set_discoverable(discoverable) {
                log::warn!("failed to set discoverable: {e}");
            }
        }
        // Don't save to config — discoverable is session-only.
        // The "discoverable on startup" setting controls the initial state.
        self.notify_frontend(FrontendEvent::DiscoverableChanged(discoverable));
    }

    fn update_settings(&mut self, settings: lan_mouse_ipc::Settings) {
        self.config.set_settings(&settings);
        if let Err(e) = self.config.write_back() {
            log::warn!("failed to write config: {e}");
        }
        self.notify_frontend(FrontendEvent::SettingsChanged(settings));
    }

    fn handle_discovery_event(&mut self, event: lan_mouse_ipc::FrontendEvent) {
        match &event {
            FrontendEvent::DiscoveredDevice {
                hostname,
                fingerprint,
                ..
            } => {
                // Auto-authorize discovered devices' fingerprints so connections work immediately
                if !fingerprint.is_empty() {
                    let keys = self.authorized_keys.read().expect("lock");
                    if !keys.contains_key(fingerprint) {
                        drop(keys);
                        log::info!("auto-authorizing discovered device: {hostname}");
                        self.add_authorized_key(hostname.clone(), fingerprint.clone());
                        self.save_config();
                    }
                }
                // Cache the event so it can be re-emitted if a matching
                // connection is later deleted.
                self.discovered_devices
                    .insert(hostname.clone(), event.clone());
                // Filter: suppress if a connection already exists for this hostname.
                if self.hostname_has_client(hostname) {
                    log::debug!(
                        "suppressing discovered device {hostname} — connection already exists"
                    );
                    return;
                }
            }
            FrontendEvent::DeviceLost { hostname } => {
                self.discovered_devices.remove(hostname);
            }
            _ => {}
        }
        self.notify_frontend(event);
    }

    fn hostname_has_client(&self, hostname: &str) -> bool {
        self.client_manager
            .clients()
            .iter()
            .any(|(c, _)| c.hostname.as_deref() == Some(hostname))
    }

    fn accept_discovered_device(
        &mut self,
        hostname: String,
        addrs: Vec<IpAddr>,
        port: u16,
        fingerprint: String,
        position: Position,
    ) {
        // Authorize the fingerprint
        self.add_authorized_key(hostname.clone(), fingerprint);

        // Create a new client with the discovered info
        let handle = self.client_manager.add_client();
        self.client_manager
            .set_hostname(handle, Some(hostname.clone()));
        self.client_manager.set_port(handle, port);
        self.client_manager.set_pos(handle, position);

        // Use discovered IPs directly so we don't depend on DNS
        if !addrs.is_empty() {
            log::info!("using discovered IPs for {hostname}: {addrs:?}");
            self.client_manager.set_fix_ips(handle, addrs);
        }

        let (c, s) = self.client_manager.get_state(handle).unwrap();
        self.notify_frontend(FrontendEvent::Created(handle, c, s));

        // Activate the client
        self.activate_client(handle);
        self.save_config();

        // Tell the frontend to drop this device from the "discovered" list,
        // since it's now a connection. The cache is kept so it can be
        // re-surfaced if the connection is later deleted.
        self.notify_frontend(FrontendEvent::DeviceLost {
            hostname: hostname.clone(),
        });

        log::info!("accepted discovered device: {hostname} at position {position}");
    }

    fn spawn_hook_command(&self, handle: ClientHandle) {
        let Some(cmd) = self.client_manager.get_enter_cmd(handle) else {
            return;
        };
        tokio::task::spawn_local(async move {
            log::info!("spawning command!");
            let mut child = match Command::new("sh").arg("-c").arg(cmd.as_str()).spawn() {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("could not execute cmd: {e}");
                    return;
                }
            };
            match child.wait().await {
                Ok(s) => {
                    if s.success() {
                        log::info!("{cmd} exited successfully");
                    } else {
                        log::warn!("{cmd} exited with {s}");
                    }
                }
                Err(e) => log::warn!("{cmd}: {e}"),
            }
        });
    }
}
