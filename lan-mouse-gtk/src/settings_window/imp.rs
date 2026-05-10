use adw::subclass::prelude::*;
use adw::{ActionRow, ComboRow, PreferencesGroup, SwitchRow, prelude::*};
use glib::subclass::InitializingObject;
use gtk::{Button, CompositeTemplate, StringList, glib};

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
    #[template_child]
    pub windows_service_group: TemplateChild<PreferencesGroup>,
    #[template_child]
    pub service_status_row: TemplateChild<ActionRow>,
    #[template_child]
    pub service_install_btn: TemplateChild<Button>,
    #[template_child]
    pub service_start_btn: TemplateChild<Button>,
    #[template_child]
    pub service_stop_btn: TemplateChild<Button>,
    #[template_child]
    pub service_uninstall_btn: TemplateChild<Button>,
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

        // Windows-only: surface the service-management panel and wire up
        // the install/start/stop/uninstall buttons. Other platforms keep
        // the group hidden (default `visible=false` from the .ui file).
        #[cfg(windows)]
        self.setup_windows_service_panel();
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

#[cfg(windows)]
impl SettingsWindow {
    fn setup_windows_service_panel(&self) {
        self.windows_service_group.set_visible(true);
        self.refresh_service_status();

        // Each control button relaunches the lan-mouse CLI elevated via
        // ShellExecuteEx("runas") — that triggers the standard Windows UAC
        // prompt. After the elevated process exits we refresh the status.
        let actions = [
            (&self.service_install_btn, "install"),
            (&self.service_start_btn, "start"),
            (&self.service_stop_btn, "stop"),
            (&self.service_uninstall_btn, "uninstall"),
        ];
        for (btn, action) in actions {
            let action: &'static str = action;
            let win = self.obj().downgrade();
            btn.connect_clicked(move |_| {
                if let Err(e) = relaunch_elevated(&["cli", "service", action]) {
                    log::warn!("failed to relaunch elevated for `{action}`: {e}");
                }
                // Allow SCM a moment to transition before we re-read.
                let win = win.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    if let Some(w) = win.upgrade() {
                        w.imp().refresh_service_status();
                    }
                });
            });
        }
    }

    fn refresh_service_status(&self) {
        use lan_mouse_service::InstalledState as S;
        let (text, install_enabled, uninstall_enabled, start_enabled, stop_enabled) =
            match lan_mouse_service::status() {
                Ok(S::NotInstalled) => ("Not installed", true, false, false, false),
                Ok(S::Stopped) => ("Installed — stopped", false, true, true, false),
                Ok(S::Running) => ("Installed — running", false, true, false, true),
                Ok(S::StartPending) => ("Starting…", false, true, false, true),
                Ok(S::StopPending) => ("Stopping…", false, true, true, false),
                Ok(S::Other) => ("Installed — unknown state", false, true, true, true),
                Err(e) => {
                    log::debug!("service status query failed: {e}");
                    (
                        "Status unavailable (try opening as administrator)",
                        true,
                        true,
                        true,
                        true,
                    )
                }
            };
        self.service_status_row.set_subtitle(text);
        self.service_install_btn.set_sensitive(install_enabled);
        self.service_uninstall_btn.set_sensitive(uninstall_enabled);
        self.service_start_btn.set_sensitive(start_enabled);
        self.service_stop_btn.set_sensitive(stop_enabled);
    }
}

#[cfg(windows)]
fn relaunch_elevated(args: &[&str]) -> Result<(), std::io::Error> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let exe = std::env::current_exe()?;
    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(once(0)).collect();
    let verb_w: Vec<u16> = "runas".encode_utf16().chain(once(0)).collect();
    let params = args.join(" ");
    let params_w: Vec<u16> = params.encode_utf16().chain(once(0)).collect();

    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb_w.as_ptr();
    info.lpFile = exe_w.as_ptr();
    info.lpParameters = params_w.as_ptr();
    info.nShow = SW_HIDE as i32;

    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // We don't wait on the process here — the caller schedules a status
    // refresh on a glib timeout. Waiting synchronously would freeze the
    // GTK main loop until the user dismissed the UAC prompt.
    if !info.hProcess.is_null() {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(info.hProcess) };
    }
    Ok(())
}
