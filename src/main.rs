// SPDX-License-Identifier: MIT

mod canvas;
mod debug;
mod decode;
mod library;
mod settings;
mod settings_window;
mod theme;
mod window;

use gtk::{gdk, gio, glib, prelude::*};

const APP_ID: &str = "dev.previewer.Previewer";

/// Action name, accelerators.
const ACCELS: &[(&str, &[&str])] = &[
    ("win.open", &["<Ctrl>o"]),
    ("win.copy", &["<Ctrl>c"]),
    ("win.settings", &["<Ctrl>comma"]),
    ("win.crop", &["<Ctrl>k"]),
    ("win.crop-apply", &["Return", "KP_Enter"]),
    ("win.undo-crop", &["<Ctrl>z"]),
    ("win.next", &["Right", "Page_Down"]),
    ("win.previous", &["Left", "Page_Up"]),
    ("win.first", &["Home"]),
    ("win.last", &["End"]),
    (
        "win.zoom-in",
        &["plus", "equal", "KP_Add", "<Ctrl>plus", "<Ctrl>equal"],
    ),
    ("win.zoom-out", &["minus", "KP_Subtract", "<Ctrl>minus"]),
    ("win.zoom-fit", &["0", "KP_0", "<Ctrl>0"]),
    ("win.zoom-actual", &["1", "KP_1", "<Ctrl>1"]),
    ("win.play-pause", &["space"]),
    ("win.rotate-left", &["<Ctrl>l"]),
    ("win.rotate-right", &["<Ctrl>r"]),
    ("win.toggle-sidebar", &["F9", "<Ctrl>b"]),
    ("win.fullscreen", &["F11", "f"]),
    ("win.leave-fullscreen", &["Escape"]),
    ("win.close", &["<Ctrl>w"]),
    ("app.quit", &["<Ctrl>q"]),
];

fn main() -> glib::ExitCode {
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("previewer {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_startup(|app| {
        settings::ensure_default_settings();
        let display = gdk::Display::default().expect("GTK opened a display");
        theme::ThemeManager::install(&display);

        let quit = gio::SimpleAction::new("quit", None);
        quit.connect_activate(glib::clone!(
            #[weak]
            app,
            move |_, _| app.quit()
        ));
        app.add_action(&quit);
        for (action, accels) in ACCELS {
            app.set_accels_for_action(action, accels);
        }
    });
    app.connect_activate(|app| window::present(app, Vec::new()));
    app.connect_open(|app, files, _| {
        let paths = files.iter().filter_map(gio::File::path).collect();
        window::present(app, paths);
    });
    app.run()
}
