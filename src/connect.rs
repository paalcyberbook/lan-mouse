use crate::client::ClientManager;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};

/// Buffer size for receiving - large enough for clipboard messages
const RECV_BUF_SIZE: usize = 65536 + 5 + MAX_EVENT_SIZE; // clipboard + header + margin
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    // Bind to matching address family for the target
    let bind_addr = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let conn = Arc::new(
        UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) enum ConnEvent {
    Proto((ClientHandle, ProtoEvent)),
    #[cfg(feature = "clipboard")]
    Clipboard(String),
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    #[cfg(feature = "clipboard")]
    clipboard_rx: Receiver<String>,
    #[cfg(feature = "clipboard")]
    clipboard_tx: Sender<String>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    ipv6_enabled: bool,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        ipv6_enabled: bool,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        #[cfg(feature = "clipboard")]
        let (clipboard_tx, clipboard_rx) = channel();
        Self {
            cert,
            client_manager,
            ipv6_enabled,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            #[cfg(feature = "clipboard")]
            clipboard_rx,
            #[cfg(feature = "clipboard")]
            clipboard_tx,
            ping_response: Default::default(),
        }
    }

    /// Poll both the proto-event and (when enabled) clipboard-text channels
    /// in a single borrow. Needed because callers with a `&mut self` to the
    /// connection can't hold two outstanding mut borrows across branches of a
    /// `tokio::select!`.
    pub(crate) async fn recv_any(&mut self) -> ConnEvent {
        #[cfg(feature = "clipboard")]
        {
            tokio::select! {
                ev = self.recv_rx.recv() => ConnEvent::Proto(ev.expect("channel closed")),
                text = self.clipboard_rx.recv() => ConnEvent::Clipboard(text.expect("channel closed")),
            }
        }
        #[cfg(not(feature = "clipboard"))]
        {
            ConnEvent::Proto(self.recv_rx.recv().await.expect("channel closed"))
        }
    }

    /// Send raw bytes to the active connection for a client (used for clipboard)
    #[cfg(feature = "clipboard")]
    pub(crate) async fn send_raw(
        &self,
        data: &[u8],
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                match conn.send(data).await {
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        log::warn!("failed to send clipboard to {addr}: {e}");
                    }
                }
            }
        }
        Err(LanMouseConnectionError::NotConnected)
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(&self.client_manager, handle, addr, &self.conns).await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                #[cfg(feature = "clipboard")]
                self.clipboard_tx.clone(),
                self.ping_response.clone(),
                self.ipv6_enabled,
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    #[cfg(feature = "clipboard")] clipboard_tx: Sender<String>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    ipv6_enabled: bool,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .filter(|a| ipv6_enabled || a.is_ipv4())
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            tx,
            #[cfg(feature = "clipboard")]
            clipboard_tx,
            ping_response.clone(),
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    #[cfg(feature = "clipboard")] clipboard_tx: Sender<String>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    let mut buf = [0u8; RECV_BUF_SIZE];
    loop {
        let n = match conn.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                // Surface the actual webrtc-util/webrtc-dtls error instead of
                // a bare "recv error". DTLSConn collapses every fatal cause —
                // peer closed, decrypt failure, underlying socket dead — into
                // an opaque `util::Error`, so the string is the only diagnosis
                // signal we get. Treating this as fatal here mirrors what the
                // old `while let Ok(n)` did; the outer reconnect loop handles
                // recovery.
                log::warn!("recv error from {addr}: {e}");
                break;
            }
        };
        // Check for clipboard message (sentinel byte 0xFF)
        #[cfg(feature = "clipboard")]
        if n >= 5 && buf[0] == crate::clipboard::CLIPBOARD_MSG_TYPE {
            if let Some(text) = crate::clipboard::decode_clipboard_msg(&buf[..n]) {
                log::debug!("received clipboard text ({} bytes) from {addr}", text.len());
                clipboard_tx.send(text).expect("clipboard channel closed");
            }
            continue;
        }
        // Pad short messages to MAX_EVENT_SIZE for the protocol parser
        let mut fixed_buf = [0u8; MAX_EVENT_SIZE];
        let copy_len = n.min(MAX_EVENT_SIZE);
        fixed_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
        if let Ok(event) = fixed_buf.try_into() {
            log::trace!("{addr} <==<==<== {event}");
            match event {
                ProtoEvent::Pong(b) => {
                    client_manager.set_active_addr(handle, Some(addr));
                    client_manager.set_alive(handle, b);
                    ping_response.borrow_mut().insert(addr);
                }
                event => tx.send((handle, event)).expect("channel closed"),
            }
        }
    }
    disconnect(&client_manager, handle, addr, &conns).await;
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.lock().await.remove(&addr);
    client_manager.set_active_addr(handle, None);
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}
