mod imp;

use adw::subclass::prelude::ObjectSubclassIsExt;
use glib::Object;
use gtk::glib;

use lan_mouse_ipc::Settings;

glib::wrapper! {
    pub struct SettingsWindow(ObjectSubclass<imp::SettingsWindow>)
        @extends adw::PreferencesWindow, adw::Window, gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget,
                    gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl SettingsWindow {
    pub fn new() -> Self {
        Object::builder().build()
    }

    pub fn apply_settings(&self, settings: &Settings) {
        self.imp().apply_settings(settings);
    }

    pub fn collect_settings(&self) -> Settings {
        self.imp().collect_settings()
    }

    pub fn set_release_bind_label(&self, keys: &str) {
        self.imp().set_release_bind_label(keys);
    }
}
