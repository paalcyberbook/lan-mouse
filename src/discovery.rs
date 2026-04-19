use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use local_channel::mpsc::{Receiver, Sender, channel};
use tokio::task::{JoinHandle, spawn_local};

use lan_mouse_ipc::{FrontendEvent, Position};

const SERVICE_TYPE: &str = "_lan-mouse._udp.local.";

pub(crate) struct DiscoveryService {
    daemon: Option<mdns_sd::ServiceDaemon>,
    hostname: String,
    port: u16,
    fingerprint: String,
    discoverable: bool,
    service_fullname: Option<String>,
    browse_task: Option<JoinHandle<()>>,
    event_tx: Sender<FrontendEvent>,
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        self.stop_all();
        if let Some(daemon) = self.daemon.take() {
            let _ = daemon.shutdown();
        }
    }
}

impl DiscoveryService {
    pub(crate) fn new(
        port: u16,
        fingerprint: String,
        hostname: String,
        discoverable: bool,
    ) -> Result<(Self, Receiver<FrontendEvent>), mdns_sd::Error> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let (event_tx, event_rx) = channel();

        let mut service = Self {
            daemon: Some(daemon),
            hostname,
            port,
            fingerprint,
            discoverable,
            service_fullname: None,
            browse_task: None,
            event_tx,
        };

        service.apply_mode()?;

        Ok((service, event_rx))
    }

    fn daemon(&self) -> &mdns_sd::ServiceDaemon {
        self.daemon.as_ref().expect("daemon shutdown")
    }

    pub(crate) fn is_discoverable(&self) -> bool {
        self.discoverable
    }

    pub(crate) fn set_discoverable(&mut self, discoverable: bool) -> Result<(), mdns_sd::Error> {
        if self.discoverable == discoverable {
            return Ok(());
        }
        self.discoverable = discoverable;
        self.apply_mode()
    }

    fn apply_mode(&mut self) -> Result<(), mdns_sd::Error> {
        self.stop_all();

        if self.discoverable {
            // ON: advertise ourselves AND browse for others
            self.start_advertising()?;
            self.start_browsing()?;
            log::info!("mDNS: discoverable ON — advertising and browsing");
        } else {
            // OFF: silent, no network activity
            log::info!("mDNS: discoverable OFF — silent");
        }
        Ok(())
    }

    fn stop_all(&mut self) {
        self.stop_advertising();
        self.stop_browsing();
    }

    fn start_advertising(&mut self) -> Result<(), mdns_sd::Error> {
        let properties = [
            ("fingerprint", self.fingerprint.as_str()),
            ("hostname", self.hostname.as_str()),
            ("port", &self.port.to_string()),
        ];
        let service_info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            &self.hostname,
            &format!("{}.local.", self.hostname),
            "",
            self.port,
            &properties[..],
        )?
        .enable_addr_auto();
        let fullname = service_info.get_fullname().to_string();
        self.daemon().register(service_info)?;
        self.service_fullname = Some(fullname);
        Ok(())
    }

    fn stop_advertising(&mut self) {
        if let Some(fullname) = self.service_fullname.take() {
            let _ = self.daemon().unregister(&fullname);
        }
    }

    fn start_browsing(&mut self) -> Result<(), mdns_sd::Error> {
        let receiver = self.daemon().browse(SERVICE_TYPE)?;
        let own_hostname = self.hostname.clone();
        let event_tx = self.event_tx.clone();

        let task = spawn_local(async move {
            let mut known_devices: HashMap<String, Instant> = HashMap::new();

            loop {
                let event = {
                    let receiver = receiver.clone();
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        tokio::task::spawn_blocking(move || receiver.recv()),
                    )
                    .await
                };

                let event = match event {
                    Ok(Ok(e)) => e,
                    Ok(Err(e)) => {
                        log::warn!("mDNS task error: {e}");
                        break;
                    }
                    Err(_) => continue, // timeout, loop back for re-register check
                };

                match event {
                    Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                        let remote_hostname = info
                            .get_property_val_str("hostname")
                            .unwrap_or_default()
                            .to_string();

                        if remote_hostname == own_hostname {
                            continue;
                        }

                        if known_devices.contains_key(&remote_hostname) {
                            continue;
                        }

                        let fingerprint = info
                            .get_property_val_str("fingerprint")
                            .unwrap_or_default()
                            .to_string();

                        let port = info
                            .get_property_val_str("port")
                            .and_then(|p| p.parse::<u16>().ok())
                            .unwrap_or(lan_mouse_ipc::DEFAULT_PORT);

                        let position_str = info
                            .get_property_val_str("position")
                            .unwrap_or_default()
                            .to_string();
                        let position = position_str
                            .parse::<Position>()
                            .unwrap_or_default()
                            .opposite();

                        let addrs: Vec<IpAddr> =
                            info.get_addresses().iter().copied().collect();

                        if fingerprint.is_empty() {
                            continue;
                        }

                        known_devices.insert(remote_hostname.clone(), Instant::now());

                        log::info!(
                            "mDNS: discovered device: {} ({:?})",
                            remote_hostname,
                            addrs
                        );

                        event_tx
                            .send(FrontendEvent::DiscoveredDevice {
                                hostname: remote_hostname,
                                addrs,
                                port,
                                fingerprint,
                                position,
                            })
                            .expect("channel closed");
                    }
                    Ok(mdns_sd::ServiceEvent::ServiceRemoved(_, fullname)) => {
                        let hostname = fullname
                            .strip_suffix(&format!(".{SERVICE_TYPE}"))
                            .unwrap_or(&fullname)
                            .to_string();

                        // Ignore our own service removal
                        if hostname == own_hostname {
                            continue;
                        }

                        if let Some(discovered_at) = known_devices.get(&hostname) {
                            // Ignore spurious removals within 10 seconds
                            if discovered_at.elapsed() < Duration::from_secs(10) {
                                log::debug!(
                                    "mDNS: ignoring spurious removal of {} ({}ms ago)",
                                    hostname,
                                    discovered_at.elapsed().as_millis()
                                );
                                continue;
                            }
                            known_devices.remove(&hostname);
                            log::info!("mDNS: device lost: {}", hostname);
                            event_tx
                                .send(FrontendEvent::DeviceLost { hostname })
                                .expect("channel closed");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("mDNS browse error: {e}");
                        break;
                    }
                }
            }
        });

        self.browse_task = Some(task);
        Ok(())
    }

    fn stop_browsing(&mut self) {
        if let Some(task) = self.browse_task.take() {
            task.abort();
            let _ = self.daemon().stop_browse(SERVICE_TYPE);
        }
    }
}
