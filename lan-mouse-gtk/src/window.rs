mod imp;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::{Object, clone};
use gtk::{
    NoSelection, gio,
    glib::{self, closure_local},
};

use lan_mouse_ipc::{
    ClientConfig, ClientHandle, ClientState, DEFAULT_PORT, FrontendRequest, FrontendRequestWriter,
    Position,
};

use crate::{authorization_window::AuthorizationWindow, fingerprint_window::FingerprintWindow};

use super::{client_object::ClientObject, client_row::ClientRow};

glib::wrapper! {
    pub struct Window(ObjectSubclass<imp::Window>)
        @extends adw::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                    gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl Window {
    pub(super) fn new(app: &adw::Application, conn: FrontendRequestWriter) -> Self {
        let window: Self = Object::builder().property("application", app).build();
        window
            .imp()
            .frontend_request_writer
            .borrow_mut()
            .replace(conn);
        window
    }

    fn clients(&self) -> gio::ListStore {
        self.imp()
            .clients
            .borrow()
            .clone()
            .expect("Could not get clients")
    }

    fn client_by_idx(&self, idx: u32) -> Option<ClientObject> {
        self.clients().item(idx).map(|o| o.downcast().unwrap())
    }

    fn row_by_idx(&self, idx: i32) -> Option<ClientRow> {
        self.imp()
            .client_list
            .get()
            .row_at_index(idx)
            .map(|o| o.downcast().expect("expected ClientRow"))
    }

    fn setup_clients(&self) {
        let model = gio::ListStore::new::<ClientObject>();
        self.imp().clients.replace(Some(model));

        let selection_model = NoSelection::new(Some(self.clients()));
        self.imp().client_list.bind_model(
            Some(&selection_model),
            clone!(
                #[weak(rename_to = window)]
                self,
                #[upgrade_or_panic]
                move |obj| {
                    let client_object = obj
                        .downcast_ref()
                        .expect("Expected object of type `ClientObject`.");
                    let row = window.create_client_row(client_object);
                    row.connect_closure(
                        "request-hostname-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, hostname: String| {
                                log::debug!("request-hostname-change");
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let hostname = Some(hostname).filter(|s| !s.is_empty());
                                    /* changed in response to FrontendEvent
                                     * -> do not request additional update */
                                    window.request(FrontendRequest::UpdateHostname(
                                        client.handle(),
                                        hostname,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-port-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, port: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::UpdatePort(
                                        client.handle(),
                                        port as u16,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-activate",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, active: bool| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    log::debug!(
                                        "request: {} client",
                                        if active { "activating" } else { "deactivating" }
                                    );
                                    window.request(FrontendRequest::Activate(
                                        client.handle(),
                                        active,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-delete",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::Delete(client.handle()));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-dns",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    window.request(FrontendRequest::ResolveDns(
                                        client.get_data().handle,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-position-change",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, pos_idx: u32| {
                                if let Some(client) = window.client_by_idx(row.index() as u32) {
                                    let position = match pos_idx {
                                        0 => Position::Left,
                                        1 => Position::Right,
                                        2 => Position::Top,
                                        _ => Position::Bottom,
                                    };
                                    window.request(FrontendRequest::UpdatePosition(
                                        client.handle(),
                                        position,
                                    ));
                                }
                            }
                        ),
                    );
                    row.connect_closure(
                        "request-send-files",
                        false,
                        closure_local!(
                            #[strong]
                            window,
                            move |row: ClientRow, paths: Vec<String>| {
                                let Some(client) = window.client_by_idx(row.index() as u32) else {
                                    return;
                                };
                                let handle = client.handle();
                                let count = paths.len();
                                for path in paths {
                                    window.request(FrontendRequest::SendFile {
                                        client: handle,
                                        path: path.into(),
                                    });
                                }
                                window.show_toast(&format!(
                                    "Queued {} file(s) for {}",
                                    count,
                                    client
                                        .get_data()
                                        .hostname
                                        .clone()
                                        .unwrap_or_else(|| "client".into())
                                ));
                            }
                        ),
                    );
                    row.upcast()
                }
            ),
        );
    }

    fn setup_icon(&self) {
        self.set_icon_name(Some("de.feschber.LanMouse"));
    }

    /// workaround for a bug in libadwaita that shows an ugly line beneath
    /// the last element if a placeholder is set.
    /// https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6308
    fn update_placeholder_visibility(&self) {
        let visible = self.clients().n_items() == 0;
        let placeholder = self.imp().client_placeholder.get();
        self.imp().client_list.set_placeholder(match visible {
            true => Some(&placeholder),
            false => None,
        });
    }

    fn create_client_row(&self, client_object: &ClientObject) -> ClientRow {
        let row = ClientRow::new(client_object);
        row.bind(client_object);
        row
    }

    pub(super) fn new_client(
        &self,
        handle: ClientHandle,
        client: ClientConfig,
        state: ClientState,
    ) {
        let client = ClientObject::new(handle, client, state.clone());
        self.clients().append(&client);
        self.update_placeholder_visibility();
        self.update_dns_state(handle, !state.ips.is_empty());
    }

    pub(super) fn update_client_list(
        &self,
        clients: Vec<(ClientHandle, ClientConfig, ClientState)>,
    ) {
        for (handle, client, state) in clients {
            if self.client_idx(handle).is_some() {
                self.update_client_config(handle, client);
                self.update_client_state(handle, state);
            } else {
                self.new_client(handle, client, state);
            }
        }
    }

    pub(super) fn update_port(&self, port: u16, msg: Option<String>) {
        if let Some(msg) = msg {
            self.show_toast(msg.as_str());
        }
        self.imp().set_port(port);
    }

    fn client_idx(&self, handle: ClientHandle) -> Option<usize> {
        self.clients()
            .iter::<ClientObject>()
            .position(|c| c.ok().map(|c| c.handle() == handle).unwrap_or_default())
    }

    pub(super) fn delete_client(&self, handle: ClientHandle) {
        let Some(idx) = self.client_idx(handle) else {
            log::warn!("could not find client with handle {handle}");
            return;
        };

        self.clients().remove(idx as u32);
        if self.clients().n_items() == 0 {
            self.update_placeholder_visibility();
        }
    }

    pub(super) fn update_client_config(&self, handle: ClientHandle, client: ClientConfig) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        row.set_hostname(client.hostname);
        row.set_port(client.port);
        row.set_position(client.pos);
    }

    pub(super) fn update_client_state(&self, handle: ClientHandle, state: ClientState) {
        let Some(row) = self.row_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };
        let Some(client_object) = self.client_object_for_handle(handle) else {
            log::warn!("could not find row for handle {handle}");
            return;
        };

        /* activation state */
        row.set_active(state.active);

        /* dns state */
        client_object.set_resolving(state.resolving);

        self.update_dns_state(handle, !state.ips.is_empty());
        let ips = state
            .ips
            .into_iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>();
        client_object.set_ips(ips);
    }

    fn client_object_for_handle(&self, handle: ClientHandle) -> Option<ClientObject> {
        self.client_idx(handle)
            .and_then(|i| self.client_by_idx(i as u32))
    }

    fn row_for_handle(&self, handle: ClientHandle) -> Option<ClientRow> {
        self.client_idx(handle)
            .and_then(|i| self.row_by_idx(i as i32))
    }

    fn update_dns_state(&self, handle: ClientHandle, resolved: bool) {
        if let Some(client_row) = self.row_for_handle(handle) {
            client_row.set_dns_state(resolved);
        }
    }

    fn request_port_change(&self) {
        let port = self
            .imp()
            .port_entry
            .get()
            .text()
            .as_str()
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
        self.request(FrontendRequest::ChangePort(port));
    }

    fn request_capture(&self) {
        self.request(FrontendRequest::EnableCapture);
    }

    fn request_emulation(&self) {
        self.request(FrontendRequest::EnableEmulation);
    }

    fn request_client_create(&self) {
        self.request(FrontendRequest::Create);
    }

    fn open_fingerprint_dialog(&self, fp: Option<String>) {
        let window = FingerprintWindow::new(fp);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: FingerprintWindow, desc: String, fp: String| {
                    parent.request_fingerprint_add(desc, fp);
                    w.close();
                }
            ),
        );
        window.present();
    }

    fn request_fingerprint_add(&self, desc: String, fp: String) {
        self.request(FrontendRequest::AuthorizeKey(desc, fp));
    }

    fn request(&self, request: FrontendRequest) {
        self.crate_request(request);
    }

    /// Sibling of [`Self::request`] visible to other modules in this crate
    /// (e.g. `file_transfer_window`). Kept under a distinct name so adding
    /// this doesn't change the existing private call sites.
    pub(crate) fn crate_request(&self, request: FrontendRequest) {
        let mut requester = self.imp().frontend_request_writer.borrow_mut();
        let requester = requester.as_mut().unwrap();
        if let Err(e) = requester.request(request) {
            log::error!("error sending message: {e}");
        };
    }

    pub(super) fn show_toast(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        let toast_overlay = &self.imp().toast_overlay;
        toast_overlay.add_toast(toast);
    }

    pub(super) fn set_capture(&self, active: bool) {
        self.imp().capture_active.replace(active);
        self.update_capture_emulation_status();
    }

    pub(super) fn set_emulation(&self, active: bool) {
        self.imp().emulation_active.replace(active);
        self.update_capture_emulation_status();
    }

    fn update_capture_emulation_status(&self) {
        let capture = self.imp().capture_active.get();
        let emulation = self.imp().emulation_active.get();
        self.imp().capture_status_row.set_visible(!capture);
        self.imp().emulation_status_row.set_visible(!emulation);
        self.imp()
            .capture_emulation_group
            .set_visible(!capture || !emulation);
    }

    pub(super) fn set_pk_fp(&self, fingerprint: &str) {
        self.imp().fingerprint_row.set_subtitle(fingerprint);
    }

    pub(super) fn set_discoverable(&self, discoverable: bool) {
        self.imp().discoverable_switch.set_active(discoverable);
    }

    pub(super) fn add_discovered_device(
        &self,
        hostname: &str,
        addrs: &[std::net::IpAddr],
        port: u16,
        fingerprint: &str,
        position: lan_mouse_ipc::Position,
    ) {
        let ip_str = addrs
            .iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        let row = adw::ActionRow::builder()
            .title(hostname)
            .subtitle(format!("{} (suggested: {})", ip_str, position))
            .build();

        let connect_btn = gtk::Button::builder()
            .label("Connect")
            .valign(gtk::Align::Center)
            .build();
        connect_btn.add_css_class("suggested-action");

        let hostname = hostname.to_string();
        let fingerprint = fingerprint.to_string();
        let addrs = addrs.to_vec();
        connect_btn.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                let body = format!(
                    "IPs: {}\nFingerprint: {}",
                    addrs
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    fingerprint
                );

                let dialog = adw::MessageDialog::new(
                    Some(&window),
                    Some(&format!("Connect to {}?", hostname)),
                    Some(&body),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("left", "Left");
                dialog.add_response("right", "Right");
                dialog.add_response("top", "Top");
                dialog.add_response("bottom", "Bottom");

                // Highlight the suggested position
                let suggested_id = format!("{position}");
                dialog.set_response_appearance(&suggested_id, adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some(&suggested_id));
                dialog.set_close_response("cancel");

                let hostname = hostname.clone();
                let fingerprint = fingerprint.clone();
                let addrs = addrs.clone();
                dialog.connect_response(
                    None,
                    clone!(
                        #[weak]
                        window,
                        move |_dialog, response| {
                            let pos = match response {
                                "left" => lan_mouse_ipc::Position::Left,
                                "right" => lan_mouse_ipc::Position::Right,
                                "top" => lan_mouse_ipc::Position::Top,
                                "bottom" => lan_mouse_ipc::Position::Bottom,
                                _ => return,
                            };
                            window.request(FrontendRequest::AcceptDiscoveredDevice {
                                hostname: hostname.clone(),
                                addrs: addrs.clone(),
                                port,
                                fingerprint: fingerprint.clone(),
                                position: pos,
                            });
                            window.show_toast(&format!("Connecting to {}", hostname));
                        }
                    ),
                );
                dialog.present();
            }
        ));

        row.add_suffix(&connect_btn);
        self.imp().discovered_list.append(&row);
    }

    pub(super) fn remove_discovered_device(&self, hostname: &str) {
        let list = &self.imp().discovered_list;
        let mut idx = 0;
        while let Some(row) = list.row_at_index(idx) {
            if let Some(action_row) = row.downcast_ref::<adw::ActionRow>() {
                if action_row.title() == hostname {
                    list.remove(&row);
                    return;
                }
            }
            idx += 1;
        }
    }

    pub(super) fn set_status(&self, msg: &str) {
        self.imp().status_label.set_text(msg);
    }

    pub(super) fn update_settings(&self, settings: &lan_mouse_ipc::Settings) {
        self.imp().current_settings.replace(settings.clone());
    }

    pub(crate) fn open_settings(&self) {
        let settings_window = crate::settings_window::SettingsWindow::new();
        settings_window.set_transient_for(Some(self));

        // Apply current settings
        let settings = self.imp().current_settings.borrow().clone();
        settings_window.apply_settings(&settings);

        // Save settings when window is closed
        settings_window.connect_close_request(clone!(
            #[weak(rename_to = window)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |settings_win| {
                let settings = settings_win.collect_settings();
                window.request(FrontendRequest::UpdateSettings(settings));
                glib::Propagation::Proceed
            }
        ));

        settings_window.present();
    }

    pub(super) fn request_authorization(&self, fingerprint: &str) {
        if let Some(w) = self.imp().authorization_window.borrow_mut().take() {
            w.close();
        }
        let window = AuthorizationWindow::new(fingerprint);
        window.set_transient_for(Some(self));
        window.connect_closure(
            "confirm-clicked",
            false,
            closure_local!(
                #[strong(rename_to = parent)]
                self,
                move |w: AuthorizationWindow, fp: String| {
                    w.close();
                    parent.open_fingerprint_dialog(Some(fp));
                }
            ),
        );
        window.connect_closure(
            "cancel-clicked",
            false,
            closure_local!(move |w: AuthorizationWindow| {
                w.close();
            }),
        );
        window.present();
        self.imp().authorization_window.replace(Some(window));
    }
}
