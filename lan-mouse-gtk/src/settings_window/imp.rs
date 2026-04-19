use adw::subclass::prelude::*;
use adw::{ActionRow, ComboRow, SwitchRow, prelude::*};
use glib::subclass::InitializingObject;
use gtk::{CompositeTemplate, StringList, glib};

use lan_mouse_ipc::Settings;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/settings_window.ui")]
pub struct SettingsWindow {
    #[template_child]
    pub discoverable_on_startup_switch: TemplateChild<SwitchRow>,
    #[template_child]
    pub auto_accept_switch: TemplateChild<SwitchRow>,
    #[template_child]
    pub auto_accept_warning: TemplateChild<ActionRow>,
    #[template_child]
    pub start_minimized_switch: TemplateChild<SwitchRow>,
    #[template_child]
    pub release_bind_row: TemplateChild<ActionRow>,
    #[template_child]
    pub ipv6_enabled_switch: TemplateChild<SwitchRow>,
    #[template_child]
    pub listen_ipv4_combo: TemplateChild<ComboRow>,
    #[template_child]
    pub listen_ipv6_combo: TemplateChild<ComboRow>,
}

#[glib::object_subclass]
impl ObjectSubclass for SettingsWindow {
    const NAME: &'static str = "LanMouseSettingsWindow";
    type Type = super::SettingsWindow;
    type ParentType = adw::PreferencesWindow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for SettingsWindow {
    fn constructed(&self) {
        self.parent_constructed();

        // Show/hide security warning based on auto-accept toggle
        let warning = self.auto_accept_warning.clone();
        self.auto_accept_switch
            .connect_notify_local(Some("active"), move |switch, _| {
                warning.set_visible(switch.is_active());
            });

        // Populate IPv4 dropdown
        let ipv4_list = StringList::new(&["All interfaces (0.0.0.0)"]);
        let ipv6_list = StringList::new(&["All interfaces ([::])"]);

        if let Ok(addrs) = if_addrs::get_if_addrs() {
            for iface in &addrs {
                if iface.is_loopback() {
                    continue;
                }
                match iface.ip() {
                    std::net::IpAddr::V4(ip) => {
                        ipv4_list.append(&format!("{} ({})", ip, iface.name));
                    }
                    std::net::IpAddr::V6(ip) => {
                        if !ip.to_string().starts_with("fe80:") {
                            ipv6_list.append(&format!("{} ({})", ip, iface.name));
                        }
                    }
                }
            }
        }

        self.listen_ipv4_combo.set_model(Some(&ipv4_list));
        self.listen_ipv6_combo.set_model(Some(&ipv6_list));
    }
}

impl WidgetImpl for SettingsWindow {}
impl WindowImpl for SettingsWindow {}
impl AdwWindowImpl for SettingsWindow {}
impl PreferencesWindowImpl for SettingsWindow {}

impl SettingsWindow {
    pub fn apply_settings(&self, settings: &Settings) {
        self.discoverable_on_startup_switch
            .set_active(settings.discoverable_on_startup);
        self.auto_accept_switch
            .set_active(settings.auto_accept_discovered);
        self.auto_accept_warning
            .set_visible(settings.auto_accept_discovered);
        self.start_minimized_switch
            .set_active(settings.start_minimized);

        self.ipv6_enabled_switch.set_active(settings.ipv6_enabled);

        // Apply listen IPv4 selection
        if let Some(ip) = &settings.listen_ipv4 {
            let ip_str = ip.to_string();
            if let Some(model) = self.listen_ipv4_combo.model() {
                let list = model.downcast_ref::<StringList>().unwrap();
                for i in 0..list.n_items() {
                    if let Some(s) = list.string(i) {
                        if s.starts_with(&ip_str) {
                            self.listen_ipv4_combo.set_selected(i);
                            break;
                        }
                    }
                }
            }
        }

        // Apply listen IPv6 selection
        if let Some(ip) = &settings.listen_ipv6 {
            let ip_str = ip.to_string();
            if let Some(model) = self.listen_ipv6_combo.model() {
                let list = model.downcast_ref::<StringList>().unwrap();
                for i in 0..list.n_items() {
                    if let Some(s) = list.string(i) {
                        if s.starts_with(&ip_str) {
                            self.listen_ipv6_combo.set_selected(i);
                            break;
                        }
                    }
                }
            }
        }

        // Format release bind keys nicely
        let keys = settings
            .release_bind
            .iter()
            .map(|k| {
                k.strip_prefix("Key")
                    .unwrap_or(k)
                    .replace("Left", "L-")
                    .replace("Right", "R-")
            })
            .collect::<Vec<_>>()
            .join(" + ");
        self.release_bind_row
            .set_subtitle(&format!("Current: {keys}"));
    }

    pub fn collect_settings(&self) -> Settings {
        let listen_ipv4 = if self.listen_ipv4_combo.selected() == 0 {
            None
        } else {
            // Parse IP from the combo text "x.x.x.x (iface)"
            let model = self.listen_ipv4_combo.model().unwrap();
            let list = model.downcast_ref::<StringList>().unwrap();
            let text = list.string(self.listen_ipv4_combo.selected()).unwrap();
            text.split(' ').next().and_then(|s| s.parse().ok())
        };

        let listen_ipv6 = if self.listen_ipv6_combo.selected() == 0 {
            None
        } else {
            let model = self.listen_ipv6_combo.model().unwrap();
            let list = model.downcast_ref::<StringList>().unwrap();
            let text = list.string(self.listen_ipv6_combo.selected()).unwrap();
            text.split(' ').next().and_then(|s| s.parse().ok())
        };

        Settings {
            discoverable_on_startup: self.discoverable_on_startup_switch.is_active(),
            auto_accept_discovered: self.auto_accept_switch.is_active(),
            start_minimized: self.start_minimized_switch.is_active(),
            listen_ipv4,
            listen_ipv6,
            ipv6_enabled: self.ipv6_enabled_switch.is_active(),
            release_bind: Vec::new(), // read-only for now, passed through from config
        }
    }

    pub fn set_release_bind_label(&self, keys: &str) {
        self.release_bind_row.set_subtitle(keys);
    }
}
