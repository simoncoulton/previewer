// SPDX-License-Identifier: MIT

//! Developer aid: render the window (and optionally its menu) to PNG files.
//!
//! `PREVIEWER_SCREENSHOT=/tmp/shot.png previewer image.png` writes the window
//! once it has had time to load, then quits. With `PREVIEWER_SCREENSHOT_MENU`
//! set the menu is opened first and saved next to it as `shot.menu.png`.
//! `PREVIEWER_ACTIONS=zoom-in,rotate-right,next` runs window actions first.

use std::{path::Path, time::Duration};

use gtk::{glib, glib::clone, graphene, prelude::*};

fn settle() -> Duration {
    std::env::var("PREVIEWER_SETTLE_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(Duration::from_millis(2500), Duration::from_millis)
}

pub fn schedule(window: &gtk::ApplicationWindow, menu: &gtk::MenuButton) {
    if let Ok(dir) = std::env::var("PREVIEWER_RECORD") {
        // Leave time to size the window from outside (e.g. `hyprctl`).
        let delay = std::env::var("PREVIEWER_RECORD_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        glib::timeout_add_local_once(
            Duration::from_millis(delay),
            clone!(
                #[weak]
                window,
                move || record(&window, Path::new(&dir))
            ),
        );
        return;
    }
    let Ok(target) = std::env::var("PREVIEWER_SCREENSHOT") else {
        return;
    };
    let settle = settle();
    let with_menu = std::env::var_os("PREVIEWER_SCREENSHOT_MENU").is_some();
    if let Ok(actions) = std::env::var("PREVIEWER_ACTIONS") {
        glib::timeout_add_local_once(
            settle / 2,
            clone!(
                #[weak]
                window,
                move || {
                    for name in actions.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                        if let Err(error) =
                            WidgetExt::activate_action(&window, &format!("win.{name}"), None)
                        {
                            eprintln!("previewer: action {name}: {error}");
                        }
                    }
                }
            ),
        );
    }
    if with_menu {
        glib::timeout_add_local_once(
            settle / 2,
            clone!(
                #[weak]
                menu,
                move || menu.popup()
            ),
        );
    }
    glib::timeout_add_local_once(
        settle,
        clone!(
            #[weak]
            window,
            #[weak]
            menu,
            move || {
                save(window.upcast_ref(), Path::new(&target));
                if with_menu && let Some(popover) = menu.popover() {
                    save(
                        popover.upcast_ref(),
                        &Path::new(&target).with_extension("menu.png"),
                    );
                }
                if let Some(app) = window.application() {
                    app.quit();
                }
            }
        ),
    );
}

fn save(widget: &gtk::Widget, path: &Path) {
    let (width, height) = (widget.width(), widget.height());
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let (Some(node), Some(renderer)) = (
        snapshot.to_node(),
        widget.native().and_then(|native| native.renderer()),
    ) else {
        eprintln!("previewer: nothing to capture for {}", path.display());
        return;
    };
    let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&bounds));
    match texture.save_to_png(path) {
        Ok(()) => eprintln!("previewer: wrote {}", path.display()),
        Err(error) => eprintln!("previewer: cannot write {}: {error}", path.display()),
    }
}

/// `PREVIEWER_RECORD=dir` writes numbered PNG frames of the window, for
/// stitching into a demo GIF, then quits. `PREVIEWER_RECORD_MS` is the
/// length, `PREVIEWER_FPS` the capture rate, and `PREVIEWER_SCRIPT` a list
/// like `1500:next,3000:zoom-in,5000:theme=nord` of `win.*` actions (or theme
/// switches) to run at those times.
fn record(window: &gtk::ApplicationWindow, dir: &Path) {
    let number = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let total = number("PREVIEWER_RECORD_MS", 10_000);
    let fps = number("PREVIEWER_FPS", 10).max(1);
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("previewer: cannot create {}: {error}", dir.display());
        return;
    }

    let script = std::env::var("PREVIEWER_SCRIPT").unwrap_or_default();
    for step in script.split(',').filter(|s| !s.trim().is_empty()) {
        let Some((at, command)) = step.split_once(':') else {
            continue;
        };
        let (Ok(at), command) = (at.trim().parse::<u64>(), command.trim().to_owned()) else {
            continue;
        };
        glib::timeout_add_local_once(
            Duration::from_millis(at),
            clone!(
                #[weak]
                window,
                move || {
                    if let Some(id) = command.strip_prefix("theme=") {
                        if let Some(manager) = crate::theme::manager() {
                            manager.select_theme(id);
                        }
                    } else if let Err(error) =
                        WidgetExt::activate_action(&window, &format!("win.{command}"), None)
                    {
                        eprintln!("previewer: action {command}: {error}");
                    }
                }
            ),
        );
    }

    let started = std::time::Instant::now();
    let frame = std::cell::Cell::new(0u32);
    let dir = dir.to_path_buf();
    glib::timeout_add_local(
        Duration::from_millis(1000 / fps),
        clone!(
            #[weak]
            window,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                let elapsed = started.elapsed();
                if elapsed.as_millis() as u64 >= total {
                    eprintln!(
                        "previewer: recorded {} frames in {:.2}s",
                        frame.get(),
                        elapsed.as_secs_f64()
                    );
                    if let Some(app) = window.application() {
                        app.quit();
                    }
                    return glib::ControlFlow::Break;
                }
                let path = dir.join(format!("frame-{:05}.png", frame.get()));
                frame.set(frame.get() + 1);
                save_quiet(window.upcast_ref(), &path);
                glib::ControlFlow::Continue
            }
        ),
    );
}

fn save_quiet(widget: &gtk::Widget, path: &Path) {
    let (width, height) = (widget.width(), widget.height());
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    if let (Some(node), Some(renderer)) = (
        snapshot.to_node(),
        widget.native().and_then(|native| native.renderer()),
    ) {
        let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
        let _ = renderer
            .render_texture(&node, Some(&bounds))
            .save_to_png(path);
    }
}
