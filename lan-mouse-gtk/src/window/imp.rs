use std::cell::{Cell, RefCell};

use adw::SwitchRow;
use adw::subclass::prelude::*;
use adw::{ActionRow, PreferencesGroup, ToastOverlay, prelude::*};
use glib::subclass::InitializingObject;
use gtk::glib::clone;
use gtk::{Button, CompositeTemplate, Entry, Label, ListBox, MenuButton, gdk, gio, glib};

use lan_mouse_ipc::{DEFAULT_PORT, FrontendRequest, FrontendRequestWriter};

use crate::authorization_window::AuthorizationWindow;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/window.ui")]
pub struct Window {
    #[template_child]
    pub fingerprint_row: TemplateChild<ActionRow>,
    #[template_child]
    pub port_edit_apply: TemplateChild<Button>,
    #[template_child]
    pub port_edit_cancel: TemplateChild<Button>,
    #[template_child]
    pub client_list: TemplateChild<ListBox>,
    #[template_child]
    pub client_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub port_entry: TemplateChild<Entry>,
    #[template_child]
    pub hostname_menu_button: TemplateChild<MenuButton>,
    #[template_child]
    pub hostname_label: TemplateChild<Label>,
    #[template_child]
    pub status_label: TemplateChild<Label>,
    #[template_child]
    pub toast_overlay: TemplateChild<ToastOverlay>,
    #[template_child]
    pub discoverable_switch: TemplateChild<SwitchRow>,
    #[template_child]
    pub discovered_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub discovered_list: TemplateChild<ListBox>,
    #[template_child]
    pub discovered_placeholder: TemplateChild<ActionRow>,
    #[template_child]
    pub capture_emulation_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub capture_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub emulation_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub input_emulation_button: TemplateChild<Button>,
    #[template_child]
    pub input_capture_button: TemplateChild<Button>,
    pub clients: RefCell<Option<gio::ListStore>>,
    pub frontend_request_writer: RefCell<Option<FrontendRequestWriter>>,
    pub port: Cell<u16>,
    pub capture_active: Cell<bool>,
    pub emulation_active: Cell<bool>,
    pub authorization_window: RefCell<Option<AuthorizationWindow>>,
    pub current_settings: RefCell<lan_mouse_ipc::Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for Window {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "LanMouseWindow";
    const ABSTRACT: bool = false;

    type Type = super::Window;
    type ParentType = adw::ApplicationWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[gtk::template_callbacks]
impl Window {
    #[template_callback]
    fn handle_add_client_pressed(&self, _button: &Button) {
        self.obj().request_client_create();
    }

    fn copy_to_clipboard(text: &str) {
        let display = gdk::Display::default().unwrap();
        let clipboard = display.clipboard();
        clipboard.set_text(text);
    }

    fn get_local_ips() -> (Vec<String>, Vec<String>) {
        let mut ipv4s = Vec::new();
        let mut ipv6s = Vec::new();
        if let Ok(addrs) = if_addrs::get_if_addrs() {
            for iface in addrs {
                if iface.is_loopback() {
                    continue;
                }
                match iface.ip() {
                    std::net::IpAddr::V4(ip) => ipv4s.push(ip.to_string()),
                    std::net::IpAddr::V6(ip) => {
                        // Skip link-local (fe80::)
                        if !ip.to_string().starts_with("fe80:") {
                            ipv6s.push(ip.to_string());
                        }
                    }
                }
            }
        }
        (ipv4s, ipv6s)
    }

    fn get_search_domain() -> Option<String> {
        #[cfg(unix)]
        {
            if let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") {
                for line in content.lines() {
                    let line = line.trim();
                    if let Some(domain) = line.strip_prefix("search ") {
                        return domain.split_whitespace().next().map(|s| s.to_string());
                    }
                    if let Some(domain) = line.strip_prefix("domain ") {
                        return domain.split_whitespace().next().map(|s| s.to_string());
                    }
                }
            }
            None
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    #[template_callback]
    fn handle_copy_fingerprint(&self, button: &Button) {
        let fingerprint: String = self.fingerprint_row.property("subtitle");
        let display = gdk::Display::default().unwrap();
        let clipboard = display.clipboard();
        clipboard.set_text(&fingerprint);
        button.set_icon_name("emblem-ok-symbolic");
        button.set_css_classes(&["success"]);
        glib::spawn_future_local(clone!(
            #[weak]
            button,
            async move {
                glib::timeout_future_seconds(1).await;
                button.set_icon_name("edit-copy-symbolic");
                button.set_css_classes(&[]);
            }
        ));
    }

    #[template_callback]
    fn handle_port_changed(&self, _entry: &Entry) {
        self.port_edit_apply.set_visible(true);
        self.port_edit_cancel.set_visible(true);
    }

    #[template_callback]
    fn handle_port_edit_apply(&self) {
        self.obj().request_port_change();
    }

    #[template_callback]
    fn handle_port_edit_cancel(&self) {
        log::debug!("cancel port edit");
        self.port_entry
            .set_text(self.port.get().to_string().as_str());
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }

    #[template_callback]
    fn handle_emulation(&self) {
        self.obj().request_emulation();
    }

    #[template_callback]
    fn handle_capture(&self) {
        self.obj().request_capture();
    }

    #[template_callback]
    fn handle_discoverable_toggled(&self) {
        let active = self.discoverable_switch.is_active();
        self.obj().request(FrontendRequest::SetDiscoverable(active));
    }

    pub fn set_port(&self, port: u16) {
        self.port.set(port);
        if port == DEFAULT_PORT {
            self.port_entry.set_text("");
        } else {
            self.port_entry.set_text(format!("{port}").as_str());
        }
        self.port_edit_apply.set_visible(false);
        self.port_edit_cancel.set_visible(false);
    }
}

impl ObjectImpl for Window {
    fn constructed(&self) {
        let hostname_str = hostname::get()
            .ok()
            .and_then(|h| h.to_str().map(|s| s.to_string()));

        if let Some(ref h) = hostname_str {
            self.hostname_label.set_text(h);
        }

        // Set tooltip with local LAN IPs
        let (ipv4s, ipv6s) = Self::get_local_ips();
        let mut tooltip_parts = Vec::new();
        if !ipv4s.is_empty() {
            tooltip_parts.push(format!("IPv4: {}", ipv4s.join(", ")));
        }
        if !ipv6s.is_empty() {
            tooltip_parts.push(format!("IPv6: {}", ipv6s.join(", ")));
        }
        if !tooltip_parts.is_empty() {
            self.hostname_label
                .set_tooltip_text(Some(&tooltip_parts.join("\n")));
        }

        // Build copy dropdown menu
        let menu = gio::Menu::new();
        if hostname_str.is_some() {
            menu.append(Some("Hostname"), Some("win.copy-hostname"));
        }
        let search_domain = Self::get_search_domain();
        if hostname_str.is_some() && search_domain.is_some() {
            menu.append(Some("Full Hostname (FQDN)"), Some("win.copy-fqdn"));
        }
        if !ipv4s.is_empty() {
            menu.append(Some("IPv4 Address"), Some("win.copy-ipv4"));
        }
        if !ipv6s.is_empty() {
            menu.append(Some("IPv6 Address"), Some("win.copy-ipv6"));
        }
        self.hostname_menu_button.set_menu_model(Some(&menu));

        // Register copy actions
        let action_group = gio::SimpleActionGroup::new();

        if let Some(ref h) = hostname_str {
            let hostname_clone = h.clone();
            let action = gio::SimpleAction::new("copy-hostname", None);
            action.connect_activate(move |_, _| {
                Self::copy_to_clipboard(&hostname_clone);
            });
            action_group.add_action(&action);
        }

        if let Some(ref h) = hostname_str {
            if let Some(ref domain) = search_domain {
                let fqdn = format!("{}.{}", h, domain);
                let action = gio::SimpleAction::new("copy-fqdn", None);
                action.connect_activate(move |_, _| {
                    Self::copy_to_clipboard(&fqdn);
                });
                action_group.add_action(&action);
            }
        }

        if let Some(ip) = ipv4s.first() {
            let ip_clone = ip.clone();
            let action = gio::SimpleAction::new("copy-ipv4", None);
            action.connect_activate(move |_, _| {
                Self::copy_to_clipboard(&ip_clone);
            });
            action_group.add_action(&action);
        }

        if let Some(ip) = ipv6s.first() {
            let ip_clone = ip.clone();
            let action = gio::SimpleAction::new("copy-ipv6", None);
            action.connect_activate(move |_, _| {
                Self::copy_to_clipboard(&ip_clone);
            });
            action_group.add_action(&action);
        }

        // Settings action
        let settings_action = gio::SimpleAction::new("open-settings", None);
        settings_action.connect_activate(clone!(
            #[weak(rename_to = window)]
            self,
            move |_, _| {
                window.obj().open_settings();
            }
        ));
        action_group.add_action(&settings_action);

        self.obj().insert_action_group("win", Some(&action_group));

        self.parent_constructed();
        self.set_port(DEFAULT_PORT);
        let obj = self.obj();
        obj.setup_icon();
        obj.setup_clients();
    }
}

impl WidgetImpl for Window {}
impl WindowImpl for Window {}
impl ApplicationWindowImpl for Window {}
impl AdwApplicationWindowImpl for Window {}
