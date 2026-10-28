// SPDX-License-Identifier: MIT

//! The viewer window: header bar, thumbnail sidebar, canvas and status bar.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gtk::{gdk, gio, glib, glib::clone, prelude::*};

use crate::{
    canvas::Canvas,
    decode::{self, DecodeError, RawImage},
    library::{self, Library},
    settings::SYSTEM_THEME_ID,
    theme::{self, ThemeKind},
};

const APP_TITLE: &str = "Previewer";
const THUMB_SIZE: u32 = 128;
const THUMB_BOX: (i32, i32) = (128, 96);
const SIDEBAR_WIDTH: i32 = 176;
const THUMB_CACHE_LIMIT: usize = 4000;

thread_local! {
    /// Owns every open window's state; callbacks only hold weak references,
    /// so this is what keeps a window working after `present` returns.
    static WINDOWS: RefCell<Vec<Rc<Window>>> = const { RefCell::new(Vec::new()) };
    static THUMB_CACHE: RefCell<HashMap<PathBuf, gdk::Texture>> = RefCell::new(HashMap::new());
}

type ActionHandler = Box<dyn Fn(&Rc<Window>)>;

/// What the status bar says about the current image.
struct Meta {
    width: u32,
    height: u32,
    format: String,
    bytes: Option<u64>,
}

struct Window {
    window: gtk::ApplicationWindow,
    header: gtk::HeaderBar,
    menu_button: gtk::MenuButton,
    status_bar: gtk::Box,
    canvas: Canvas,
    stack: gtk::Stack,
    error_title: gtk::Label,
    error_detail: gtk::Label,
    revealer: gtk::Revealer,
    sidebar_toggle: gtk::ToggleButton,
    play_button: gtk::Button,
    model: gtk::StringList,
    selection: gtk::SingleSelection,
    list: gtk::ListView,
    title_name: gtk::Label,
    title_sub: gtk::Label,
    status_info: gtk::Label,
    status_zoom: gtk::Label,
    library: RefCell<Library>,
    meta: RefCell<Option<Meta>>,
    /// Bumped for every load so a slow decode never overwrites a newer one.
    generation: Cell<u64>,
    /// Set while the selection is moved by code rather than by the user.
    syncing_selection: Cell<bool>,
}

/// Opens a window showing `paths` (or the empty state when none).
pub fn present(app: &gtk::Application, paths: Vec<PathBuf>) {
    let window = Window::build(app);
    WINDOWS.with(|windows| windows.borrow_mut().push(window.clone()));
    window.window.connect_destroy(clone!(
        #[weak]
        window,
        move |_| WINDOWS.with(|windows| windows.borrow_mut().retain(|w| !Rc::ptr_eq(w, &window)))
    ));
    window.show_paths(&paths);
    window.window.present();
    crate::debug::schedule(&window.window, &window.menu_button);
}

impl Window {
    fn build(app: &gtk::Application) -> Rc<Self> {
        let settings = theme::manager().map(|m| m.settings()).unwrap_or_default();

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(APP_TITLE)
            .default_width(1100)
            .default_height(760)
            .build();
        window.add_css_class("pv-window");

        // ---- header bar ----
        let header = gtk::HeaderBar::new();
        header.add_css_class("pv-header");
        let title_name = gtk::Label::builder()
            .label(APP_TITLE)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .max_width_chars(40)
            .build();
        title_name.add_css_class("title-name");
        let title_sub = gtk::Label::builder().visible(false).build();
        title_sub.add_css_class("title-sub");
        let title_block = gtk::Box::new(gtk::Orientation::Vertical, 0);
        title_block.add_css_class("title-block");
        title_block.set_valign(gtk::Align::Center);
        title_block.append(&title_name);
        title_block.append(&title_sub);
        header.set_title_widget(Some(&title_block));
        window.set_titlebar(Some(&header));

        let sidebar_toggle = gtk::ToggleButton::new();
        style_button(
            &sidebar_toggle,
            "sidebar-show-symbolic",
            "Toggle Sidebar (F9)",
        );
        sidebar_toggle.set_active(settings.show_sidebar);
        header.pack_start(&sidebar_toggle);
        header.pack_start(&action_button(
            "document-open-symbolic",
            "Open… (Ctrl+O)",
            "win.open",
        ));
        header.pack_start(&action_button(
            "go-previous-symbolic",
            "Previous Image (←)",
            "win.previous",
        ));
        header.pack_start(&action_button(
            "go-next-symbolic",
            "Next Image (→)",
            "win.next",
        ));

        let menu_button = gtk::MenuButton::new();
        menu_button.add_css_class("pv-menubutton");
        menu_button.set_icon_name("open-menu-symbolic");
        menu_button.set_tooltip_text(Some("Menu"));
        menu_button
            .first_child()
            .expect("a menu button has an inner button")
            .add_css_class("pv-button");
        header.pack_end(&menu_button);
        for (icon, tip, action) in [
            (
                "object-rotate-right-symbolic",
                "Rotate Right (Ctrl+R)",
                "win.rotate-right",
            ),
            (
                "object-rotate-left-symbolic",
                "Rotate Left (Ctrl+L)",
                "win.rotate-left",
            ),
            (
                "zoom-original-symbolic",
                "Actual Size (1)",
                "win.zoom-actual",
            ),
            ("zoom-fit-best-symbolic", "Zoom to Fit (0)", "win.zoom-fit"),
            ("zoom-in-symbolic", "Zoom In (+)", "win.zoom-in"),
            ("zoom-out-symbolic", "Zoom Out (−)", "win.zoom-out"),
        ] {
            header.pack_end(&action_button(icon, tip, action));
        }
        // Only animated images need it; refresh_status shows and labels it.
        let play_button = action_button(
            "media-playback-pause-symbolic",
            "Pause (Space)",
            "win.play-pause",
        );
        play_button.set_visible(false);
        header.pack_end(&play_button);

        // ---- stage ----
        let canvas = Canvas::new();
        canvas.set_scroll_zooms(settings.scroll_zooms);
        canvas.set_checkerboard(settings.checkerboard);

        let stack = gtk::Stack::new();
        stack.add_css_class("stage");
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(120);
        stack.add_named(&empty_state(), Some("empty"));
        stack.add_named(&canvas, Some("image"));
        let (error_page, error_title, error_detail) = error_state();
        stack.add_named(&error_page, Some("error"));
        stack.set_visible_child_name("empty");

        // ---- sidebar ----
        let model = gtk::StringList::new(&[]);
        let selection = gtk::SingleSelection::new(Some(model.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(false);
        let list = gtk::ListView::new(Some(selection.clone()), Some(thumbnail_factory()));
        list.add_css_class("thumb-list");
        list.set_focusable(false);
        list.set_focus_on_click(false);
        list.set_single_click_activate(false);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_width(SIDEBAR_WIDTH)
            .child(&list)
            .build();
        scroller.add_css_class("sidebar");
        scroller.add_css_class("pv-scroll");
        scroller.set_focusable(false);
        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideRight)
            .transition_duration(160)
            .reveal_child(false)
            .child(&scroller)
            .build();

        // ---- status bar ----
        let status_info = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        status_info.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let status_zoom = gtk::Label::builder().xalign(1.0).build();
        status_zoom.add_css_class("zoom-label");
        let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        status_bar.add_css_class("status-bar");
        status_bar.append(&status_info);
        status_bar.append(&status_zoom);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&revealer);
        body.append(&stack);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&body);
        root.append(&status_bar);
        window.set_child(Some(&root));

        let this = Rc::new(Self {
            window,
            header,
            menu_button: menu_button.clone(),
            status_bar,
            canvas,
            stack,
            error_title,
            error_detail,
            revealer,
            sidebar_toggle,
            play_button,
            model,
            selection,
            list,
            title_name,
            title_sub,
            status_info,
            status_zoom,
            library: RefCell::new(Library::default()),
            meta: RefCell::new(None),
            generation: Cell::new(0),
            syncing_selection: Cell::new(false),
        });

        this.install_actions();
        this.install_menu(&menu_button);
        this.install_drop_target();
        this.connect_signals();
        this.refresh_actions();
        this.refresh_status();
        this
    }

    // ---- wiring ---------------------------------------------------------

    fn connect_signals(self: &Rc<Self>) {
        self.sidebar_toggle.connect_toggled(clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.refresh_sidebar()
        ));

        self.canvas.connect_view_changed(clone!(
            #[weak(rename_to = this)]
            self,
            move || this.refresh_status()
        ));

        self.selection.connect_selected_notify(clone!(
            #[weak(rename_to = this)]
            self,
            move |selection| {
                if this.syncing_selection.get() {
                    return;
                }
                this.go_to(selection.selected() as usize);
            }
        ));

        self.window.connect_fullscreened_notify(clone!(
            #[weak(rename_to = this)]
            self,
            move |window| {
                let chrome = !window.is_fullscreen();
                this.header.set_visible(chrome);
                this.status_bar.set_visible(chrome);
            }
        ));

        theme::connect_changed(clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                if let Some(manager) = theme::manager() {
                    let settings = manager.settings();
                    this.canvas.set_scroll_zooms(settings.scroll_zooms);
                    this.canvas.set_checkerboard(settings.checkerboard);
                }
                glib::ControlFlow::Continue
            }
        ));
    }

    fn install_actions(self: &Rc<Self>) {
        let add = |name: &str, handler: ActionHandler| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(clone!(
                #[weak(rename_to = this)]
                self,
                move |_, _| handler(&this)
            ));
            self.window.add_action(&action);
        };

        add("open", Box::new(|this| this.choose_files()));
        add("copy", Box::new(|this| this.copy_image()));
        add(
            "next",
            Box::new(|this| {
                let next = this.library.borrow().next_index();
                if let Some(index) = next {
                    this.go_to(index);
                }
            }),
        );
        add(
            "previous",
            Box::new(|this| {
                let previous = this.library.borrow().previous_index();
                if let Some(index) = previous {
                    this.go_to(index);
                }
            }),
        );
        add("first", Box::new(|this| this.go_to(0)));
        add(
            "last",
            Box::new(|this| {
                let last = this.library.borrow().last_index();
                if let Some(index) = last {
                    this.go_to(index);
                }
            }),
        );
        add("zoom-in", Box::new(|this| this.canvas.zoom_in()));
        add("zoom-out", Box::new(|this| this.canvas.zoom_out()));
        add("zoom-fit", Box::new(|this| this.canvas.zoom_fit()));
        add("zoom-actual", Box::new(|this| this.canvas.zoom_actual()));
        add("rotate-left", Box::new(|this| this.canvas.rotate(false)));
        add("rotate-right", Box::new(|this| this.canvas.rotate(true)));
        add("play-pause", Box::new(|this| this.canvas.toggle_playback()));
        add(
            "toggle-sidebar",
            Box::new(|this| {
                if this.sidebar_toggle.is_sensitive() {
                    this.sidebar_toggle
                        .set_active(!this.sidebar_toggle.is_active());
                }
            }),
        );
        add(
            "fullscreen",
            Box::new(|this| {
                this.window.set_fullscreened(!this.window.is_fullscreen());
            }),
        );
        add(
            "leave-fullscreen",
            Box::new(|this| this.window.set_fullscreened(false)),
        );
        add("close", Box::new(|this| this.window.close()));
    }

    /// Enables or disables actions to match what can currently be done.
    fn refresh_actions(&self) {
        let set = |name: &str, enabled: bool| {
            if let Some(action) = self
                .window
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(enabled);
            }
        };
        let library = self.library.borrow();
        let has_image = self.canvas.has_image();
        for name in [
            "copy",
            "zoom-in",
            "zoom-out",
            "zoom-fit",
            "zoom-actual",
            "rotate-left",
            "rotate-right",
        ] {
            set(name, has_image);
        }
        set("play-pause", self.canvas.is_animated());
        set("previous", library.previous_index().is_some());
        set("next", library.next_index().is_some());
        set("first", library.index() > 0);
        set(
            "last",
            library
                .last_index()
                .is_some_and(|last| library.index() < last),
        );
    }

    /// Shows the sidebar only when there is more than one image to browse.
    fn refresh_sidebar(&self) {
        let several = self.library.borrow().len() > 1;
        self.sidebar_toggle.set_sensitive(several);
        self.revealer
            .set_reveal_child(several && self.sidebar_toggle.is_active());
    }

    fn refresh_status(&self) {
        let info = match self.meta.borrow().as_ref() {
            Some(meta) => {
                let mut parts = vec![format!("{} × {} px", meta.width, meta.height)];
                parts.push(meta.format.clone());
                if self.canvas.is_animated() {
                    let frames = self.canvas.frame_count();
                    let state = if self.canvas.is_playing() {
                        ""
                    } else {
                        ", paused"
                    };
                    parts.push(match frames {
                        0 => format!("animated{state}"),
                        n => format!("{n} frames{state}"),
                    });
                }
                if let Some(bytes) = meta.bytes {
                    parts.push(human_size(bytes));
                }
                parts.join("  ·  ")
            }
            None => String::new(),
        };
        self.status_info.set_text(&info);
        self.play_button.set_visible(self.canvas.is_animated());
        let (icon, tip) = if self.canvas.is_playing() {
            ("media-playback-pause-symbolic", "Pause (Space)")
        } else {
            ("media-playback-start-symbolic", "Play (Space)")
        };
        self.play_button.set_icon_name(icon);
        self.play_button.set_tooltip_text(Some(tip));
        self.status_zoom.set_text(
            &self
                .canvas
                .zoom_percent()
                .map(|percent| format!("{percent:.0}%"))
                .unwrap_or_default(),
        );
        self.refresh_actions();
    }

    fn install_drop_target(self: &Rc<Self>) {
        let target = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY);
        target.set_types(&[gdk::FileList::static_type(), gio::File::static_type()]);
        target.connect_enter(clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            gdk::DragAction::empty(),
            move |_, _, _| {
                this.stack.add_css_class("drop-active");
                gdk::DragAction::COPY
            }
        ));
        target.connect_leave(clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.stack.remove_css_class("drop-active")
        ));
        target.connect_drop(clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            false,
            move |_, value, _, _| {
                this.stack.remove_css_class("drop-active");
                let files: Vec<gio::File> = if let Ok(list) = value.get::<gdk::FileList>() {
                    list.files()
                } else if let Ok(file) = value.get::<gio::File>() {
                    vec![file]
                } else {
                    return false;
                };
                let paths: Vec<PathBuf> = files.iter().filter_map(gio::File::path).collect();
                if paths.is_empty() {
                    return false;
                }
                this.show_paths(&paths);
                true
            }
        ));
        self.window.add_controller(target);
    }

    // ---- menu -----------------------------------------------------------

    fn install_menu(self: &Rc<Self>, button: &gtk::MenuButton) {
        let popover = gtk::Popover::new();
        popover.add_css_class("pv-popover");
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::End);
        // Rebuilt on every open so new theme files and the active theme show.
        popover.connect_show(clone!(
            #[weak(rename_to = this)]
            self,
            move |popover| popover.set_child(Some(&this.menu_content(popover)))
        ));
        button.set_popover(Some(&popover));
    }

    fn menu_content(self: &Rc<Self>, popover: &gtk::Popover) -> gtk::Box {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_width_request(240);

        content.append(&menu_item(
            popover,
            "Open…",
            Some("Ctrl+O"),
            Some("win.open"),
            false,
        ));
        content.append(&menu_item(
            popover,
            "Copy Image",
            Some("Ctrl+C"),
            Some("win.copy"),
            false,
        ));
        if self.canvas.is_animated() {
            let label = if self.canvas.is_playing() {
                "Pause Animation"
            } else {
                "Play Animation"
            };
            content.append(&menu_item(
                popover,
                label,
                Some("Space"),
                Some("win.play-pause"),
                false,
            ));
        }
        content.append(&menu_item(
            popover,
            "Full Screen",
            Some("F11"),
            Some("win.fullscreen"),
            false,
        ));
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let heading = gtk::Label::new(Some("Theme"));
        heading.add_css_class("menu-heading");
        heading.set_xalign(0.0);
        content.append(&heading);

        let current = theme::manager()
            .map(|m| m.settings().theme)
            .unwrap_or_else(|| SYSTEM_THEME_ID.to_owned());
        let themes = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let mut last_kind = ThemeKind::System;
        for info in theme::available_themes() {
            if info.kind != last_kind {
                themes.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                last_kind = info.kind.clone();
            }
            let item = menu_item(popover, &info.name, None, None, info.id == current);
            let id = info.id.clone();
            item.connect_clicked(move |_| {
                if let Some(manager) = theme::manager() {
                    manager.select_theme(&id);
                }
            });
            themes.append(&item);
        }
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(300)
            .child(&themes)
            .build();
        scroller.add_css_class("pv-scroll");
        content.append(&scroller);
        content
    }

    // ---- opening --------------------------------------------------------

    fn choose_files(self: &Rc<Self>) {
        let images = gtk::FileFilter::new();
        images.set_name(Some("Images"));
        images.add_mime_type("image/*");
        let everything = gtk::FileFilter::new();
        everything.set_name(Some("All Files"));
        everything.add_pattern("*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&images);
        filters.append(&everything);

        let dialog = gtk::FileDialog::builder()
            .title("Open Images")
            .modal(true)
            .filters(&filters)
            .default_filter(&images)
            .build();
        dialog.open_multiple(
            Some(&self.window),
            gio::Cancellable::NONE,
            clone!(
                #[weak(rename_to = this)]
                self,
                move |result| {
                    let Ok(files) = result else {
                        return;
                    };
                    let paths: Vec<PathBuf> = (0..files.n_items())
                        .filter_map(|i| files.item(i).and_downcast::<gio::File>())
                        .filter_map(|file| file.path())
                        .collect();
                    if !paths.is_empty() {
                        this.show_paths(&paths);
                    }
                }
            ),
        );
    }

    /// Replaces what the window shows with `paths`, see [`Library::open`].
    fn show_paths(self: &Rc<Self>, paths: &[PathBuf]) {
        let library = Library::open(paths);
        let names: Vec<String> = library
            .files()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        self.syncing_selection.set(true);
        self.model.splice(0, self.model.n_items(), &refs);
        self.syncing_selection.set(false);

        let empty = library.is_empty();
        *self.library.borrow_mut() = library;
        self.refresh_sidebar();

        if empty {
            self.generation.set(self.generation.get() + 1);
            self.canvas.set_image(None, 1.0, false);
            *self.meta.borrow_mut() = None;
            self.title_name.set_text(APP_TITLE);
            self.title_sub.set_visible(false);
            self.window.set_title(Some(APP_TITLE));
            if paths.is_empty() {
                self.stack.set_visible_child_name("empty");
            } else {
                self.show_error("No images found", "Nothing here Previewer can open.");
            }
            self.refresh_status();
        } else {
            self.load_current();
        }
    }

    fn go_to(self: &Rc<Self>, index: usize) {
        if self.library.borrow_mut().select(index) {
            self.load_current();
        }
    }

    /// Starts decoding the library's current file.
    fn load_current(self: &Rc<Self>) {
        let (path, index, count) = {
            let library = self.library.borrow();
            let Some(path) = library.current().cloned() else {
                return;
            };
            (path, library.index(), library.len())
        };
        let generation = self.generation.get() + 1;
        self.generation.set(generation);

        // Keep the sidebar on the current image without re-entering `go_to`.
        self.syncing_selection.set(true);
        self.selection.set_selected(index as u32);
        self.syncing_selection.set(false);
        self.list
            .scroll_to(index as u32, gtk::ListScrollFlags::NONE, None);

        let name = library::file_name(&path);
        self.title_name.set_text(&name);
        self.window
            .set_title(Some(&format!("{name} — {APP_TITLE}")));
        if count > 1 {
            self.title_sub
                .set_text(&format!("{} of {count}", index + 1));
            self.title_sub.set_visible(true);
        } else {
            self.title_sub.set_visible(false);
        }
        self.refresh_actions();

        let bytes = std::fs::metadata(&path).ok().map(|m| m.len());
        glib::spawn_future_local(clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                let decode_path = path.clone();
                let result = gio::spawn_blocking(move || decode::decode(&decode_path)).await;
                if this.generation.get() != generation {
                    return;
                }
                let result = result
                    .unwrap_or_else(|_| Err(DecodeError::Failed("the decoder crashed".to_owned())));
                this.finish_load(&path, bytes, result);
            }
        ));
    }

    fn finish_load(
        &self,
        path: &std::path::Path,
        bytes: Option<u64>,
        result: Result<RawImage, DecodeError>,
    ) {
        match result {
            Ok(mut raw) => {
                let width = (f64::from(raw.width) / raw.pixel_scale).round().max(1.0) as u32;
                let height = (f64::from(raw.height) / raw.pixel_scale).round().max(1.0) as u32;
                let meta = Meta {
                    width,
                    height,
                    format: raw.format.clone(),
                    bytes,
                };
                let (pixel_scale, has_alpha) = (raw.pixel_scale, raw.has_alpha);
                if let Some((delay, stream)) = raw.stream.take() {
                    let autoplay = theme::manager().is_none_or(|m| !m.settings().reduce_motion);
                    self.canvas.set_stream(
                        raw.into_texture(),
                        delay,
                        stream,
                        pixel_scale,
                        has_alpha,
                        autoplay,
                    );
                    self.show_loaded(meta);
                } else if raw.animation.is_empty() {
                    self.show_image(raw.into_texture(), pixel_scale, has_alpha, meta);
                } else {
                    let autoplay = theme::manager().is_none_or(|m| !m.settings().reduce_motion);
                    let frames = raw.frame_textures();
                    self.canvas
                        .set_animation(frames, pixel_scale, has_alpha, autoplay);
                    self.show_loaded(meta);
                }
            }
            Err(error) => match decode::load_with_gdk(path) {
                Ok((texture, format)) => {
                    let meta = Meta {
                        width: texture.width() as u32,
                        height: texture.height() as u32,
                        format,
                        bytes,
                    };
                    self.show_image(texture, 1.0, true, meta);
                }
                Err(gdk_error) => {
                    let detail = match error {
                        DecodeError::Unsupported => {
                            format!("This file type is not supported on this system.\n{gdk_error}")
                        }
                        DecodeError::Failed(message) => message,
                    };
                    self.show_error("Can't open this image", &detail);
                }
            },
        }
    }

    fn show_image(&self, texture: gdk::Texture, pixel_scale: f64, has_alpha: bool, meta: Meta) {
        self.canvas.set_image(Some(texture), pixel_scale, has_alpha);
        self.show_loaded(meta);
    }

    /// Updates the chrome once the canvas holds the new image.
    fn show_loaded(&self, meta: Meta) {
        let single = self.library.borrow().len() <= 1;
        if single {
            self.title_sub
                .set_text(&format!("{} × {}", meta.width, meta.height));
            self.title_sub.set_visible(true);
        }
        *self.meta.borrow_mut() = Some(meta);
        self.stack.set_visible_child_name("image");
        self.refresh_status();
    }

    fn show_error(&self, title: &str, detail: &str) {
        self.canvas.set_image(None, 1.0, false);
        *self.meta.borrow_mut() = None;
        self.error_title.set_text(title);
        self.error_detail.set_text(detail);
        self.stack.set_visible_child_name("error");
        self.refresh_status();
    }

    fn copy_image(&self) {
        if let Some(texture) = self.canvas.texture() {
            WidgetExt::display(&self.window)
                .clipboard()
                .set_texture(&texture);
        }
    }
}

// ---- widgets ------------------------------------------------------------

fn style_button(button: &impl IsA<gtk::Button>, icon: &str, tooltip: &str) {
    let button = button.as_ref();
    button.add_css_class("pv-button");
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
    button.set_focusable(false);
}

fn action_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    let button = gtk::Button::new();
    style_button(&button, icon, tooltip);
    button.set_action_name(Some(action));
    button
}

/// A row in the popover menu. `checked` reserves and fills the check column.
fn menu_item(
    popover: &gtk::Popover,
    label: &str,
    shortcut: Option<&str>,
    action: Option<&str>,
    checked: bool,
) -> gtk::Button {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    if action.is_none() {
        let check = gtk::Image::from_icon_name("object-select-symbolic");
        check.add_css_class("menu-check");
        row.append(&check);
    }
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    row.append(&text);
    if let Some(shortcut) = shortcut {
        let hint = gtk::Label::new(Some(shortcut));
        hint.add_css_class("menu-shortcut");
        row.append(&hint);
    }

    let button = gtk::Button::new();
    button.add_css_class("menu-item");
    button.set_child(Some(&row));
    if checked {
        button.add_css_class("active");
    }
    if let Some(action) = action {
        button.set_action_name(Some(action));
    }
    button.connect_clicked(clone!(
        #[weak]
        popover,
        move |_| popover.popdown()
    ));
    button
}

fn empty_state() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.add_css_class("empty-state");
    page.set_halign(gtk::Align::Center);
    page.set_valign(gtk::Align::Center);
    page.append(&gtk::Image::from_icon_name("image-x-generic-symbolic"));
    let title = gtk::Label::new(Some("Open an image"));
    title.add_css_class("empty-title");
    page.append(&title);
    let hint = gtk::Label::new(Some("Press Ctrl+O, or drop images here"));
    hint.add_css_class("empty-hint");
    page.append(&hint);
    page
}

/// Icon, title, detail; the two labels are returned for later updates.
fn error_state() -> (gtk::Box, gtk::Label, gtk::Label) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.add_css_class("error-state");
    page.set_halign(gtk::Align::Center);
    page.set_valign(gtk::Align::Center);
    page.append(&gtk::Image::from_icon_name("dialog-error-symbolic"));
    let title = gtk::Label::new(Some("Can't open this image"));
    title.add_css_class("error-title");
    page.append(&title);
    let detail = gtk::Label::new(None);
    detail.add_css_class("error-detail");
    detail.set_wrap(true);
    detail.set_max_width_chars(60);
    detail.set_justify(gtk::Justification::Center);
    detail.set_selectable(true);
    detail.set_focusable(false);
    page.append(&detail);
    (page, title, detail)
}

// ---- thumbnails ---------------------------------------------------------

fn thumbnail_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    // One flag per bound row, flipped off when the row is reused so queued
    // thumbnail decodes for rows that scrolled away are skipped.
    let wanted: Rc<RefCell<HashMap<usize, Arc<AtomicBool>>>> = Rc::default();

    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_can_shrink(true);
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("thumb");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_size_request(THUMB_BOX.0, THUMB_BOX.1);
        frame.set_halign(gtk::Align::Center);
        picture.set_vexpand(true);
        picture.set_hexpand(true);
        frame.append(&picture);

        let name = gtk::Label::new(None);
        name.add_css_class("thumb-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        name.set_max_width_chars(20);

        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        row.append(&frame);
        row.append(&name);
        item.set_child(Some(&row));
    });

    factory.connect_bind(clone!(
        #[strong]
        wanted,
        move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().expect("a ListItem");
            let Some(path) = item
                .item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| PathBuf::from(s.string().as_str()))
            else {
                return;
            };
            let Some(row) = item.child().and_downcast::<gtk::Box>() else {
                return;
            };
            let Some(picture) = row
                .first_child()
                .and_then(|frame| frame.first_child())
                .and_downcast::<gtk::Picture>()
            else {
                return;
            };
            if let Some(label) = row.last_child().and_downcast::<gtk::Label>() {
                label.set_text(&library::file_name(&path));
            }

            if let Some(texture) = THUMB_CACHE.with(|c| c.borrow().get(&path).cloned()) {
                picture.set_paintable(Some(&texture));
                return;
            }
            picture.set_paintable(None::<&gdk::Paintable>);

            let flag = Arc::new(AtomicBool::new(true));
            let key = item.as_ptr() as usize;
            if let Some(old) = wanted.borrow_mut().insert(key, flag.clone()) {
                old.store(false, Ordering::Relaxed);
            }
            let item = item.clone();
            glib::spawn_future_local(async move {
                let worker_path = path.clone();
                let worker_flag = flag.clone();
                let result = gio::spawn_blocking(move || {
                    if !worker_flag.load(Ordering::Relaxed) {
                        return None;
                    }
                    decode::thumbnail(&worker_path, THUMB_SIZE).ok()
                })
                .await;
                let Ok(Some(raw)) = result else {
                    return;
                };
                let texture = raw.into_texture();
                THUMB_CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    if cache.len() >= THUMB_CACHE_LIMIT {
                        cache.clear();
                    }
                    cache.insert(path.clone(), texture.clone());
                });
                // Only show it if this row still holds the same file.
                let still_bound = flag.load(Ordering::Relaxed)
                    && item
                        .item()
                        .and_downcast::<gtk::StringObject>()
                        .is_some_and(|s| std::path::Path::new(s.string().as_str()) == path);
                if still_bound {
                    picture.set_paintable(Some(&texture));
                }
            });
        }
    ));

    factory.connect_unbind(clone!(
        #[strong]
        wanted,
        move |_, item| {
            if let Some(flag) = wanted.borrow_mut().remove(&(item.as_ptr() as usize)) {
                flag.store(false, Ordering::Relaxed);
            }
        }
    ));
    factory
}

// ---- helpers ------------------------------------------------------------

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 KB");
        assert_eq!(human_size(2_300_000), "2.3 MB");
        assert_eq!(human_size(5_000_000_000), "5.0 GB");
    }
}
