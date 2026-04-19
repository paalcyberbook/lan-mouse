mod authorization_window;
mod client_object;
mod client_row;
mod fingerprint_window;
mod key_object;
mod key_row;
mod settings_window;
mod window;

use std::{env, process, str};

use window::Window;

use lan_mouse_ipc::FrontendEvent;

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
}

pub fn run() -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    #[cfg(windows)]
    let ret = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(gtk_main)
        .unwrap()
        .join()
        .unwrap();
    #[cfg(not(windows))]
    let ret = gtk_main();

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

fn gtk_main() -> glib::ExitCode {
    gio::resources_register_include!("lan-mouse.gresource").expect("Failed to register resources.");

    let app = Application::builder()
        .application_id("de.feschber.LanMouse")
        .build();

    app.connect_startup(|app| {
        load_icons();
        setup_actions(app);
        setup_menu(app);
    });
    app.connect_activate(build_ui);

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/de/feschber/LanMouse/icons");
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            app.quit();
        }
    });
    app.add_action(&quit_action);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    menu.append_submenu(Some("_File"), &file_menu);

    app.set_menubar(Some(&menu))
}

fn build_ui(app: &Application) {
    log::debug!("connecting to lan-mouse-socket");
    let (mut frontend_rx, frontend_tx) = match lan_mouse_ipc::connect() {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("{e}");
            process::exit(1);
        }
    };
    log::debug!("connected to lan-mouse-socket");

    let (sender, receiver) = async_channel::bounded(10);

    gio::spawn_blocking(move || {
        while let Some(e) = frontend_rx.next_event() {
            match e {
                Ok(e) => sender.send_blocking(e).unwrap(),
                Err(e) => {
                    log::error!("{e}");
                    break;
                }
            }
        }
    });

    let window = Window::new(app, frontend_tx);

    glib::spawn_future_local(clone!(
        #[weak]
        window,
        async move {
            loop {
                let notify = receiver.recv().await.unwrap_or_else(|_| process::exit(1));
                match notify {
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => {
                        let active: bool = s.into();
                        window.set_capture(active);
                        if active {
                            window.set_status("Input capture enabled");
                        } else {
                            window.set_status("Input capture disabled");
                        }
                    }
                    FrontendEvent::EmulationStatus(s) => {
                        let active: bool = s.into();
                        window.set_emulation(active);
                        if active {
                            window.set_status("Input emulation enabled");
                        } else {
                            window.set_status("Input emulation disabled");
                        }
                    }
                    FrontendEvent::AuthorizedUpdated(_) => {} // auth handled via popup
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        window.request_authorization(&fingerprint);
                    }
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(format!("device connected: {addr}").as_str());
                        window.set_status(&format!("Connected to {addr}"));
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos,
                    } => {
                        window.show_toast(format!("device entered: {addr} ({pos})").as_str());
                        window.set_status(&format!("Controlling device at {addr} ({pos})"));
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.show_toast(format!("{addr} disconnected").as_str());
                        window.set_status(&format!("Disconnected from {addr}"));
                    }
                    FrontendEvent::DiscoveredDevice {
                        hostname,
                        addrs,
                        port,
                        fingerprint,
                        position,
                    } => {
                        window.add_discovered_device(
                            &hostname, &addrs, port, &fingerprint, position,
                        );
                        window.set_status(&format!("Discovered: {hostname}"));
                    }
                    FrontendEvent::DeviceLost { hostname } => {
                        window.remove_discovered_device(&hostname);
                        window.set_status(&format!("Device lost: {hostname}"));
                    }
                    FrontendEvent::DiscoverableChanged(discoverable) => {
                        window.set_discoverable(discoverable);
                    }
                    FrontendEvent::SettingsChanged(settings) => {
                        window.update_settings(&settings);
                    }
                }
            }
        }
    ));

    window.present();
}
