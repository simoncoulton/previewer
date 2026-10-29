// SPDX-License-Identifier: MIT

//! The settings window: a section list on the left, the chosen section on the
//! right. Every control saves as it changes (see [`settings::Setting`]) and the
//! theme manager's reload restyles the app, so there is no apply button.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::{glib, glib::clone, prelude::*};

use crate::{
    settings::{SYSTEM_THEME_ID, Setting, Settings},
    theme::{self, ThemeInfo, ThemeKind, ThemeTokens},
};

const WINDOW_SIZE: (i32, i32) = (820, 600);
const NAV_WIDTH: i32 = 200;
const SWATCH_SIZE: (i32, i32) = (132, 78);
/// How long a slider or text box rests before its value is saved.
const SAVE_DELAY_MS: u64 = 150;
const FONT_SAVE_DELAY_MS: u64 = 500;

thread_local! {
    /// The one open settings window; callbacks only hold weak references.
    static OPEN: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

/// Runs a closure once the user has stopped changing a control for a moment,
/// so dragging a slider writes the file once instead of on every pixel.
#[derive(Default)]
struct Debounce {
    source: RefCell<Option<glib::SourceId>>,
    pending: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Debounce {
    fn schedule(self: &Rc<Self>, delay_ms: u64, action: impl FnOnce() + 'static) {
        if let Some(id) = self.source.take() {
            id.remove();
        }
        *self.pending.borrow_mut() = Some(Box::new(action));
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
            if let Some(this) = weak.upgrade() {
                // This source has fired; forget it so it is never removed twice.
                this.source.take();
                this.flush();
            }
        });
        *self.source.borrow_mut() = Some(id);
    }

    /// Runs the waiting action now, if there is one.
    fn flush(&self) {
        if let Some(id) = self.source.take() {
            id.remove();
        }
        let action = self.pending.take();
        if let Some(action) = action {
            action();
        }
    }

    fn is_pending(&self) -> bool {
        self.pending.borrow().is_some()
    }
}

struct ThemeCard {
    id: String,
    button: gtk::Button,
    swatch: gtk::DrawingArea,
    tokens: Rc<RefCell<ThemeTokens>>,
}

struct Ui {
    window: gtk::Window,
    themes_box: gtk::Box,
    cards: RefCell<Vec<ThemeCard>>,
    roundness: gtk::Scale,
    text_size: gtk::SpinButton,
    font: gtk::Entry,
    scroll_zooms: gtk::Switch,
    checkerboard: gtk::Switch,
    show_sidebar: gtk::Switch,
    reduce_motion: gtk::Switch,
    roundness_save: Rc<Debounce>,
    text_size_save: Rc<Debounce>,
    font_save: Rc<Debounce>,
    /// Set while the controls are updated from the settings, so that is not
    /// mistaken for the user changing them.
    syncing: Cell<bool>,
}

/// The settings window, if it is open.
pub fn open_window() -> Option<gtk::Window> {
    OPEN.with(|open| open.borrow().as_ref().map(|ui| ui.window.clone()))
}

/// Shows the settings window, or raises it if it is already open.
pub fn present(parent: &gtk::Window) {
    if let Some(ui) = OPEN.with(|open| open.borrow().clone()) {
        ui.window.present();
        return;
    }
    let ui = Ui::build(parent);
    OPEN.with(|open| *open.borrow_mut() = Some(ui.clone()));
    ui.window.connect_destroy(|_| {
        OPEN.with(|open| open.borrow_mut().take());
    });
    ui.window.present();
}

impl Ui {
    fn build(parent: &gtk::Window) -> Rc<Self> {
        let window = gtk::Window::builder()
            .title("Settings")
            .transient_for(parent)
            .default_width(WINDOW_SIZE.0)
            .default_height(WINDOW_SIZE.1)
            .build();
        window.add_css_class("pv-window");

        let header = gtk::HeaderBar::new();
        header.add_css_class("pv-header");
        let title = gtk::Label::new(Some("Settings"));
        title.add_css_class("title-name");
        header.set_title_widget(Some(&title));
        window.set_titlebar(Some(&header));

        // ---- appearance ----
        let themes_box = gtk::Box::new(gtk::Orientation::Vertical, 14);
        themes_box.add_css_class("theme-grid");
        let roundness = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        roundness.add_css_class("pv-scale");
        roundness.set_draw_value(true);
        roundness.set_value_pos(gtk::PositionType::Right);
        roundness.set_digits(0);
        roundness.set_width_request(220);
        let text_size = gtk::SpinButton::with_range(8.0, 32.0, 1.0);
        text_size.add_css_class("pv-entry");
        let font = gtk::Entry::new();
        font.add_css_class("pv-entry");
        font.set_placeholder_text(Some("System default"));
        font.set_width_chars(22);

        let appearance = page(
            "Appearance",
            &[
                section("Theme", &[themes_box.clone().upcast()]),
                section(
                    "Interface",
                    &[
                        row("Corner roundness", "From square to very round", &roundness),
                        row("Text size", "Interface text, in pixels", &text_size),
                        row(
                            "Interface font",
                            "A font family name, such as \"Inter\"",
                            &font,
                        ),
                    ],
                ),
            ],
        );

        // ---- viewing ----
        let scroll_zooms = switch();
        let checkerboard = switch();
        let show_sidebar = switch();
        let reduce_motion = switch();
        let viewing = page(
            "Viewing",
            &[
                section(
                    "Images",
                    &[
                        row(
                            "Wheel zooms",
                            "Zoom with the mouse wheel instead of panning. Ctrl + wheel always zooms",
                            &scroll_zooms,
                        ),
                        row(
                            "Transparency checkerboard",
                            "Draw a checkerboard behind transparent images",
                            &checkerboard,
                        ),
                    ],
                ),
                section(
                    "Window",
                    &[
                        row(
                            "Thumbnail sidebar",
                            "Show the sidebar when a window opens",
                            &show_sidebar,
                        ),
                        row(
                            "Reduce motion",
                            "Turn off interface animations",
                            &reduce_motion,
                        ),
                    ],
                ),
            ],
        );

        // ---- navigation ----
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);
        stack.set_transition_duration(120);
        stack.add_named(&appearance, Some("appearance"));
        stack.add_named(&viewing, Some("viewing"));

        let nav = gtk::ListBox::new();
        nav.add_css_class("nav-list");
        nav.set_selection_mode(gtk::SelectionMode::Single);
        for (name, icon, label) in [
            (
                "appearance",
                "preferences-desktop-appearance-symbolic",
                "Appearance",
            ),
            ("viewing", "image-x-generic-symbolic", "Viewing"),
        ] {
            nav.append(&nav_row(name, icon, label));
        }
        nav.connect_row_selected(clone!(
            #[weak]
            stack,
            move |_, row| {
                if let Some(row) = row {
                    stack.set_visible_child_name(row.widget_name().as_str());
                }
            }
        ));
        if let Some(first) = nav.row_at_index(0) {
            nav.select_row(Some(&first));
        }
        let nav_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        nav_box.add_css_class("settings-nav");
        nav_box.set_size_request(NAV_WIDTH, -1);
        nav_box.append(&nav);

        let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        body.append(&nav_box);
        body.append(&stack);
        window.set_child(Some(&body));

        let this = Rc::new(Self {
            window,
            themes_box,
            cards: RefCell::new(Vec::new()),
            roundness,
            text_size,
            font,
            scroll_zooms,
            checkerboard,
            show_sidebar,
            reduce_motion,
            roundness_save: Rc::default(),
            text_size_save: Rc::default(),
            font_save: Rc::default(),
            syncing: Cell::new(false),
        });
        this.connect_signals();
        this.refresh();
        this
    }

    fn connect_signals(self: &Rc<Self>) {
        for (switch, make) in [
            (&self.scroll_zooms, Setting::ScrollZooms as fn(bool) -> _),
            (&self.checkerboard, Setting::Checkerboard),
            (&self.show_sidebar, Setting::ShowSidebar),
            (&self.reduce_motion, Setting::ReduceMotion),
        ] {
            switch.connect_active_notify(clone!(
                #[weak(rename_to = this)]
                self,
                move |switch| {
                    if !this.syncing.get() {
                        apply(make(switch.is_active()));
                    }
                }
            ));
        }

        self.roundness.connect_value_changed(clone!(
            #[weak(rename_to = this)]
            self,
            move |scale| {
                if !this.syncing.get() {
                    let value = scale.value().round() as u32;
                    this.roundness_save.schedule(SAVE_DELAY_MS, move || {
                        apply(Setting::CornerRoundness(value))
                    });
                }
            }
        ));

        self.text_size.connect_value_changed(clone!(
            #[weak(rename_to = this)]
            self,
            move |spin| {
                if !this.syncing.get() {
                    let value = spin.value().round() as u32;
                    this.text_size_save
                        .schedule(SAVE_DELAY_MS, move || apply(Setting::TextSize(value)));
                }
            }
        ));

        self.font.connect_changed(clone!(
            #[weak(rename_to = this)]
            self,
            move |entry| {
                if !this.syncing.get() {
                    let text = entry.text().trim().to_owned();
                    this.font_save.schedule(FONT_SAVE_DELAY_MS, move || {
                        apply(Setting::InterfaceFont((!text.is_empty()).then_some(&text)));
                    });
                }
            }
        ));
        self.font.connect_activate(clone!(
            #[weak(rename_to = this)]
            self,
            move |_| this.font_save.flush()
        ));

        // Don't lose a change made in the last moments before closing.
        self.window.connect_close_request(clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_| {
                this.roundness_save.flush();
                this.text_size_save.flush();
                this.font_save.flush();
                glib::Propagation::Proceed
            }
        ));

        let close = gtk::ShortcutController::new();
        for trigger in ["Escape", "<Ctrl>w"] {
            close.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::CallbackAction::new(|widget, _| {
                    if let Some(window) = widget.downcast_ref::<gtk::Window>() {
                        window.close();
                    }
                    glib::Propagation::Stop
                })),
            ));
        }
        self.window.add_controller(close);

        theme::connect_changed(clone!(
            #[weak(rename_to = this)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move || {
                this.refresh();
                glib::ControlFlow::Continue
            }
        ));
    }

    /// Brings every control in line with the saved settings.
    fn refresh(self: &Rc<Self>) {
        let settings = theme::manager().map(|m| m.settings()).unwrap_or_default();
        self.syncing.set(true);

        // A control being dragged or typed in is ahead of the file; leave it.
        if !self.roundness_save.is_pending() {
            self.roundness
                .set_value(f64::from(settings.corner_roundness));
        }
        if !self.text_size_save.is_pending() {
            self.text_size.set_value(f64::from(settings.text_size));
        }
        if !self.font_save.is_pending() {
            let font = settings.interface_font.clone().unwrap_or_default();
            if self.font.text() != font {
                self.font.set_text(&font);
            }
        }
        self.scroll_zooms.set_active(settings.scroll_zooms);
        self.checkerboard.set_active(settings.checkerboard);
        self.show_sidebar.set_active(settings.show_sidebar);
        self.reduce_motion.set_active(settings.reduce_motion);

        self.refresh_themes(&settings);
        self.syncing.set(false);
    }

    fn refresh_themes(self: &Rc<Self>, settings: &Settings) {
        let themes = theme::available_themes();
        let unchanged = {
            let cards = self.cards.borrow();
            cards.len() == themes.len() && cards.iter().zip(&themes).all(|(c, t)| c.id == t.id)
        };
        if !unchanged {
            self.rebuild_themes(&themes);
        }

        let active = theme::current_tokens();
        for card in self.cards.borrow().iter() {
            let selected = card.id == settings.theme;
            if selected {
                card.button.add_css_class("active");
                // The active theme may have just changed on disk, and "system"
                // follows the desktop, so take its colours from the live ones.
                if let Some(tokens) = &active {
                    *card.tokens.borrow_mut() = tokens.clone();
                }
            } else {
                card.button.remove_css_class("active");
            }
            card.swatch.queue_draw();
        }
    }

    fn rebuild_themes(self: &Rc<Self>, themes: &[ThemeInfo]) {
        while let Some(child) = self.themes_box.first_child() {
            self.themes_box.remove(&child);
        }
        let mut cards = Vec::new();
        for (kind, heading, hint) in [
            (
                ThemeKind::System,
                "System",
                "Follows the colours your desktop publishes in ~/.config/gtk-4.0",
            ),
            (ThemeKind::BuiltIn, "Built in", ""),
            (
                ThemeKind::Custom,
                "Yours",
                "From ~/.config/previewer/themes",
            ),
        ] {
            let group: Vec<&ThemeInfo> = themes.iter().filter(|t| t.kind == kind).collect();
            if group.is_empty() {
                continue;
            }
            let label = gtk::Label::new(Some(heading));
            label.add_css_class("group-heading");
            label.set_xalign(0.0);
            self.themes_box.append(&label);
            if !hint.is_empty() {
                let hint = gtk::Label::new(Some(hint));
                hint.add_css_class("dim");
                hint.set_xalign(0.0);
                hint.set_wrap(true);
                self.themes_box.append(&hint);
            }
            let flow = gtk::FlowBox::new();
            flow.set_selection_mode(gtk::SelectionMode::None);
            flow.set_homogeneous(true);
            flow.set_min_children_per_line(2);
            flow.set_max_children_per_line(6);
            flow.set_column_spacing(12);
            flow.set_row_spacing(12);
            flow.set_halign(gtk::Align::Start);
            for info in group {
                let card = theme_card(info);
                flow.append(&card.button);
                if let Some(child) = card.button.parent() {
                    child.set_focusable(false);
                }
                cards.push(card);
            }
            self.themes_box.append(&flow);
        }
        *self.cards.borrow_mut() = cards;
    }
}

fn apply(setting: Setting) {
    if let Some(manager) = theme::manager() {
        manager.apply(setting);
    }
}

// ---- widgets ------------------------------------------------------------

fn nav_row(name: &str, icon: &str, label: &str) -> gtk::ListBoxRow {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.append(&gtk::Image::from_icon_name(icon));
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    content.append(&text);
    let row = gtk::ListBoxRow::new();
    row.add_css_class("nav-item");
    // The stack page this row shows.
    row.set_widget_name(name);
    row.set_child(Some(&content));
    row
}

/// A scrolling page: a title, then sections.
fn page(title: &str, sections: &[gtk::Box]) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 22);
    content.add_css_class("settings-page");
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("page-title");
    heading.set_xalign(0.0);
    content.append(&heading);
    for section in sections {
        content.append(section);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&content)
        .build();
    scroller.add_css_class("pv-scroll");
    scroller
}

/// A small heading over a card holding `rows`.
fn section(title: &str, rows: &[gtk::Widget]) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("section-heading");
    heading.set_xalign(0.0);
    section.append(&heading);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("pv-card");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        card.append(row);
    }
    section.append(&card);
    section
}

/// A labelled setting with its control on the right.
fn row(title: &str, subtitle: &str, control: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    let name = gtk::Label::new(Some(title));
    name.add_css_class("setting-title");
    name.set_xalign(0.0);
    text.append(&name);
    let hint = gtk::Label::new(Some(subtitle));
    hint.add_css_class("dim");
    hint.add_css_class("setting-hint");
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    text.append(&hint);

    control.set_valign(gtk::Align::Center);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    row.add_css_class("setting-row");
    row.append(&text);
    row.append(control);
    row.upcast()
}

fn switch() -> gtk::Switch {
    let switch = gtk::Switch::new();
    switch.add_css_class("pv-switch");
    switch
}

/// A clickable preview of a theme with its name underneath.
fn theme_card(info: &ThemeInfo) -> ThemeCard {
    let tokens = Rc::new(RefCell::new(theme::resolve(&info.id)));

    let swatch = gtk::DrawingArea::new();
    swatch.set_content_width(SWATCH_SIZE.0);
    swatch.set_content_height(SWATCH_SIZE.1);
    swatch.set_draw_func(clone!(
        #[strong]
        tokens,
        move |_, cr, width, height| draw_swatch(cr, width, height, &tokens.borrow())
    ));

    let name = gtk::Label::new(Some(&info.name));
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    name.set_max_width_chars(16);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.append(&swatch);
    content.append(&name);

    let button = gtk::Button::new();
    button.add_css_class("theme-card");
    button.set_child(Some(&content));
    if info.id == SYSTEM_THEME_ID {
        button.set_tooltip_text(Some("Follow the desktop's colours"));
    }
    let id = info.id.clone();
    button.connect_clicked(move |_| {
        if let Some(manager) = theme::manager() {
            manager.select_theme(&id);
        }
    });
    ThemeCard {
        id: info.id.clone(),
        button,
        swatch,
        tokens,
    }
}

fn set_source(cr: &gtk::cairo::Context, color: theme::Rgb) {
    cr.set_source_rgb(
        f64::from(color.0) / 255.0,
        f64::from(color.1) / 255.0,
        f64::from(color.2) / 255.0,
    );
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// A miniature of the app in the theme's colours: header bar, sidebar and an
/// image on the stage.
fn draw_swatch(cr: &gtk::cairo::Context, width: i32, height: i32, tokens: &ThemeTokens) {
    let (w, h) = (f64::from(width), f64::from(height));
    let header = 16.0;
    let side = 30.0;

    rounded_rect(cr, 0.0, 0.0, w, h, 6.0);
    cr.clip();

    set_source(cr, tokens.background);
    cr.paint().ok();

    set_source(cr, tokens.surface);
    cr.rectangle(0.0, 0.0, w, header);
    cr.fill().ok();
    cr.rectangle(0.0, header, side, h - header);
    cr.fill().ok();

    // Header: an accent dot and a title.
    set_source(cr, tokens.accent);
    cr.arc(9.0, header / 2.0, 3.0, 0.0, std::f64::consts::TAU);
    cr.fill().ok();
    set_source(cr, tokens.dim_text);
    rounded_rect(cr, w / 2.0 - 16.0, header / 2.0 - 1.5, 32.0, 3.0, 1.5);
    cr.fill().ok();

    // Sidebar: a selected thumbnail and a plain one.
    set_source(cr, tokens.highlight);
    rounded_rect(cr, 5.0, header + 6.0, side - 10.0, 14.0, 3.0);
    cr.fill().ok();
    set_source(cr, tokens.muted);
    rounded_rect(cr, 5.0, header + 24.0, side - 10.0, 14.0, 3.0);
    cr.fill().ok();

    // Stage: an "image" with an accent sun and a line of text under it.
    set_source(cr, tokens.muted);
    rounded_rect(
        cr,
        side + 10.0,
        header + 8.0,
        w - side - 20.0,
        h - header - 28.0,
        3.0,
    );
    cr.fill().ok();
    set_source(cr, tokens.accent);
    cr.arc(side + 24.0, header + 20.0, 4.0, 0.0, std::f64::consts::TAU);
    cr.fill().ok();
    set_source(cr, tokens.text);
    rounded_rect(cr, side + 10.0, h - 12.0, 36.0, 3.0, 1.5);
    cr.fill().ok();

    cr.reset_clip();
    // A hairline so a theme close to the card colour still reads as a card.
    set_source(cr, tokens.border);
    cr.set_line_width(1.0);
    rounded_rect(cr, 0.5, 0.5, w - 1.0, h - 1.0, 6.0);
    cr.stroke().ok();
}
