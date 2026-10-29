// SPDX-License-Identifier: MIT

//! The image view: zoom, pan, rotate and the transparency checkerboard.
//!
//! "Zoom" is device pixels per logical image pixel, so 1.0 is true 100% on any
//! display scale. A logical image pixel is one texture pixel, except for
//! vector images rasterised larger than their nominal size (`pixel_scale`).

use std::{cell::Cell, cell::RefCell, rc::Rc, time::Duration};

use gtk::{gdk, glib, glib::clone, graphene, prelude::*, subclass::prelude::*};

use crate::{decode::FrameStream, theme};

pub const MIN_ZOOM: f64 = 0.01;
pub const MAX_ZOOM: f64 = 64.0;
const ZOOM_STEP: f64 = 1.25;
/// Scroll-wheel notches are multiplied by this to get pixels of panning.
const WHEEL_PAN_PIXELS: f64 = 40.0;
/// Zoom change per scroll unit: exp(0.1) is about 10.5% per wheel notch.
const SCROLL_ZOOM_RATE: f64 = 0.1;
/// Past this many device pixels per texture pixel, stop smoothing.
const NEAREST_ABOVE: f64 = 4.0;
const CHECKER_SQUARE: f32 = 8.0;
/// Reach of a crop handle, and its drawn size, in CSS pixels.
const CROP_GRAB: f64 = 10.0;
const CROP_HANDLE: f32 = 9.0;
/// How much of the image outside the crop selection is dimmed.
const CROP_DIM: f32 = 0.55;

/// Pure view maths, kept apart from GTK so it can be tested.
pub mod geometry {
    use super::{MAX_ZOOM, MIN_ZOOM};

    #[derive(Clone, Copy, Debug)]
    pub struct Viewport {
        /// Widget size in CSS pixels.
        pub width: f64,
        pub height: f64,
        /// Device pixels per CSS pixel.
        pub scale: f64,
    }

    /// The zoom at which the whole image fits. Never enlarges past 100%.
    pub fn fit_zoom(logical: (f64, f64), view: Viewport) -> f64 {
        if logical.0 <= 0.0 || logical.1 <= 0.0 || view.width <= 0.0 || view.height <= 0.0 {
            return 1.0;
        }
        let fit = (view.width * view.scale / logical.0).min(view.height * view.scale / logical.1);
        fit.min(1.0).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// Displayed image size in CSS pixels.
    pub fn displayed(logical: (f64, f64), zoom: f64, view: Viewport) -> (f64, f64) {
        (logical.0 * zoom / view.scale, logical.1 * zoom / view.scale)
    }

    /// Keeps the image covering the viewport on any axis where it is larger,
    /// and centred on any axis where it is smaller.
    pub fn clamp_offset(
        offset: (f64, f64),
        logical: (f64, f64),
        zoom: f64,
        view: Viewport,
    ) -> (f64, f64) {
        let (dw, dh) = displayed(logical, zoom, view);
        let limit = |displayed: f64, viewport: f64| ((displayed - viewport) / 2.0).max(0.0);
        let (lx, ly) = (limit(dw, view.width), limit(dh, view.height));
        (offset.0.clamp(-lx, lx), offset.1.clamp(-ly, ly))
    }

    /// The offset that keeps the image point under `anchor` (CSS pixels from
    /// the viewport's top-left) fixed while the zoom changes.
    pub fn offset_after_zoom(
        offset: (f64, f64),
        old_zoom: f64,
        new_zoom: f64,
        anchor: (f64, f64),
        logical: (f64, f64),
        view: Viewport,
    ) -> (f64, f64) {
        let from_center = (anchor.0 - view.width / 2.0, anchor.1 - view.height / 2.0);
        let ratio = if old_zoom > 0.0 {
            new_zoom / old_zoom
        } else {
            1.0
        };
        let moved = (
            from_center.0 - (from_center.0 - offset.0) * ratio,
            from_center.1 - (from_center.1 - offset.1) * ratio,
        );
        clamp_offset(moved, logical, new_zoom, view)
    }

    /// One zoom step, landing exactly on 100% when a step would cross it.
    pub fn step_zoom(zoom: f64, factor: f64) -> f64 {
        let next = (zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom < 1.0 && next > 1.0) || (zoom > 1.0 && next < 1.0) {
            1.0
        } else {
            next
        }
    }

    /// A crop selection in displayed image pixels, `x0 <= x1` and `y0 <= y1`.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Selection {
        pub x0: f64,
        pub y0: f64,
        pub x1: f64,
        pub y1: f64,
    }

    /// Which part of a selection a drag holds.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Grab {
        Move,
        /// The edges being dragged: a corner holds one of each axis.
        Edges {
            left: bool,
            right: bool,
            top: bool,
            bottom: bool,
        },
    }

    /// Smallest selection edge, in image pixels.
    const MIN_SELECTION: f64 = 1.0;

    impl Selection {
        pub fn width(&self) -> f64 {
            self.x1 - self.x0
        }

        pub fn height(&self) -> f64 {
            self.y1 - self.y0
        }

        /// The rectangle between two points, kept inside `bounds`.
        pub fn between(a: (f64, f64), b: (f64, f64), bounds: (f64, f64)) -> Self {
            let clamp = |v: f64, max: f64| v.clamp(0.0, max);
            let (ax, bx) = (clamp(a.0, bounds.0), clamp(b.0, bounds.0));
            let (ay, by) = (clamp(a.1, bounds.1), clamp(b.1, bounds.1));
            Self {
                x0: ax.min(bx),
                y0: ay.min(by),
                x1: ax.max(bx),
                y1: ay.max(by),
            }
        }

        /// The whole image.
        pub fn full(bounds: (f64, f64)) -> Self {
            Self::between((0.0, 0.0), bounds, bounds)
        }

        /// What a press at `point` grabs, or `None` outside the selection.
        /// `tolerance` is the reach of an edge or corner handle.
        pub fn hit(&self, point: (f64, f64), tolerance: f64) -> Option<Grab> {
            let near = |v: f64, edge: f64| (v - edge).abs() <= tolerance;
            let within_x = point.0 >= self.x0 - tolerance && point.0 <= self.x1 + tolerance;
            let within_y = point.1 >= self.y0 - tolerance && point.1 <= self.y1 + tolerance;
            if !within_x || !within_y {
                return None;
            }
            let (left, right) = (near(point.0, self.x0), near(point.0, self.x1));
            let (top, bottom) = (near(point.1, self.y0), near(point.1, self.y1));
            // On a narrow selection both edges of an axis can be in reach;
            // take the nearer so the handles never overlap.
            let left_wins = (point.0 - self.x0).abs() <= (point.0 - self.x1).abs();
            let top_wins = (point.1 - self.y0).abs() <= (point.1 - self.y1).abs();
            let (left, right) = (left && left_wins, right && !left_wins);
            let (top, bottom) = (top && top_wins, bottom && !top_wins);
            if left || right || top || bottom {
                Some(Grab::Edges {
                    left,
                    right,
                    top,
                    bottom,
                })
            } else if point.0 >= self.x0
                && point.0 <= self.x1
                && point.1 >= self.y0
                && point.1 <= self.y1
            {
                Some(Grab::Move)
            } else {
                None
            }
        }

        /// This selection after `grab` has been dragged by `delta`, kept
        /// inside `bounds` and never thinner than one pixel.
        pub fn dragged(&self, grab: Grab, delta: (f64, f64), bounds: (f64, f64)) -> Self {
            match grab {
                Grab::Move => {
                    let dx = delta.0.clamp(-self.x0, bounds.0 - self.x1);
                    let dy = delta.1.clamp(-self.y0, bounds.1 - self.y1);
                    Self {
                        x0: self.x0 + dx,
                        y0: self.y0 + dy,
                        x1: self.x1 + dx,
                        y1: self.y1 + dy,
                    }
                }
                Grab::Edges {
                    left,
                    right,
                    top,
                    bottom,
                } => {
                    let mut next = *self;
                    if left {
                        next.x0 = (self.x0 + delta.0).clamp(0.0, self.x1 - MIN_SELECTION);
                    }
                    if right {
                        next.x1 = (self.x1 + delta.0).clamp(self.x0 + MIN_SELECTION, bounds.0);
                    }
                    if top {
                        next.y0 = (self.y0 + delta.1).clamp(0.0, self.y1 - MIN_SELECTION);
                    }
                    if bottom {
                        next.y1 = (self.y1 + delta.1).clamp(self.y0 + MIN_SELECTION, bounds.1);
                    }
                    next
                }
            }
        }
    }

    /// The texture rectangle `(x, y, width, height)` a selection covers.
    /// `selection` is in displayed texels (after `quarter_turns` clockwise
    /// turns) and `texture` is the unrotated texture size.
    pub fn texture_rect(
        selection: Selection,
        quarter_turns: u8,
        texture: (u32, u32),
    ) -> (u32, u32, u32, u32) {
        let (tw, th) = (f64::from(texture.0), f64::from(texture.1));
        let Selection { x0, y0, x1, y1 } = selection;
        let (rx0, ry0, rx1, ry1) = match quarter_turns % 4 {
            0 => (x0, y0, x1, y1),
            1 => (y0, th - x1, y1, th - x0),
            2 => (tw - x1, th - y1, tw - x0, th - y0),
            _ => (tw - y1, x0, tw - y0, x1),
        };
        let left = rx0.round().clamp(0.0, tw - 1.0);
        let top = ry0.round().clamp(0.0, th - 1.0);
        let right = rx1.round().clamp(left + 1.0, tw);
        let bottom = ry1.round().clamp(top + 1.0, th);
        (
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const BOUNDS: (f64, f64) = (100.0, 50.0);

        fn sel(x0: f64, y0: f64, x1: f64, y1: f64) -> Selection {
            Selection { x0, y0, x1, y1 }
        }

        #[test]
        fn selection_between_points_is_ordered_and_clamped() {
            assert_eq!(
                Selection::between((80.0, 40.0), (-10.0, 10.0), BOUNDS),
                sel(0.0, 10.0, 80.0, 40.0)
            );
            assert_eq!(
                Selection::between((10.0, 10.0), (500.0, 500.0), BOUNDS),
                sel(10.0, 10.0, 100.0, 50.0)
            );
        }

        #[test]
        fn press_picks_handle_or_move() {
            let s = sel(20.0, 10.0, 80.0, 40.0);
            assert_eq!(s.hit((50.0, 25.0), 4.0), Some(Grab::Move));
            assert_eq!(s.hit((0.0, 0.0), 4.0), None);
            assert_eq!(
                s.hit((20.0, 25.0), 4.0),
                Some(Grab::Edges {
                    left: true,
                    right: false,
                    top: false,
                    bottom: false
                })
            );
            assert_eq!(
                s.hit((81.0, 41.0), 4.0),
                Some(Grab::Edges {
                    left: false,
                    right: true,
                    top: false,
                    bottom: true
                })
            );
        }

        #[test]
        fn narrow_selection_prefers_the_nearer_edge() {
            let s = sel(50.0, 10.0, 53.0, 40.0);
            assert_eq!(
                s.hit((52.0, 25.0), 4.0),
                Some(Grab::Edges {
                    left: false,
                    right: true,
                    top: false,
                    bottom: false
                })
            );
        }

        #[test]
        fn moving_stays_inside_the_image() {
            let s = sel(20.0, 10.0, 80.0, 40.0);
            assert_eq!(
                s.dragged(Grab::Move, (500.0, -500.0), BOUNDS),
                sel(40.0, 0.0, 100.0, 30.0)
            );
        }

        #[test]
        fn resizing_cannot_invert_or_escape() {
            let s = sel(20.0, 10.0, 80.0, 40.0);
            let left = Grab::Edges {
                left: true,
                right: false,
                top: false,
                bottom: false,
            };
            assert_eq!(s.dragged(left, (500.0, 0.0), BOUNDS).x0, 79.0);
            assert_eq!(s.dragged(left, (-500.0, 0.0), BOUNDS).x0, 0.0);
            let corner = Grab::Edges {
                left: false,
                right: true,
                top: false,
                bottom: true,
            };
            assert_eq!(
                s.dragged(corner, (500.0, 500.0), BOUNDS),
                sel(20.0, 10.0, 100.0, 50.0)
            );
        }

        #[test]
        fn texture_rect_undoes_rotation() {
            // A 100x50 texture; the selection is its top-left 10x20 as shown.
            let s = sel(0.0, 0.0, 10.0, 20.0);
            assert_eq!(texture_rect(s, 0, (100, 50)), (0, 0, 10, 20));
            // Turned clockwise the displayed image is 50 wide and 100 tall, and
            // its top-left corner is the texture's bottom-left.
            assert_eq!(texture_rect(s, 1, (100, 50)), (0, 40, 20, 10));
            assert_eq!(texture_rect(s, 2, (100, 50)), (90, 30, 10, 20));
            // Counter-clockwise, the top-left is the texture's top-right.
            assert_eq!(texture_rect(s, 3, (100, 50)), (80, 0, 20, 10));
        }

        #[test]
        fn texture_rect_is_never_empty() {
            let s = sel(30.0, 30.0, 30.0, 30.0);
            let (_, _, w, h) = texture_rect(s, 0, (100, 50));
            assert!(w >= 1 && h >= 1);
        }

        const VIEW: Viewport = Viewport {
            width: 800.0,
            height: 600.0,
            scale: 1.0,
        };

        #[test]
        fn fit_shrinks_large_images() {
            assert_eq!(fit_zoom((1600.0, 600.0), VIEW), 0.5);
            assert_eq!(fit_zoom((800.0, 1200.0), VIEW), 0.5);
        }

        #[test]
        fn fit_does_not_enlarge_small_images() {
            assert_eq!(fit_zoom((100.0, 100.0), VIEW), 1.0);
        }

        #[test]
        fn fit_accounts_for_display_scale() {
            let hidpi = Viewport { scale: 2.0, ..VIEW };
            // 1600x1200 device pixels of room: an 1600x1200 image fits at 100%.
            assert_eq!(fit_zoom((1600.0, 1200.0), hidpi), 1.0);
            assert_eq!(fit_zoom((3200.0, 2400.0), hidpi), 0.5);
        }

        #[test]
        fn fit_survives_degenerate_sizes() {
            assert_eq!(fit_zoom((0.0, 0.0), VIEW), 1.0);
            assert_eq!(
                fit_zoom((100.0, 100.0), Viewport { width: 0.0, ..VIEW }),
                1.0
            );
        }

        #[test]
        fn small_images_are_centred() {
            let offset = clamp_offset((50.0, -70.0), (400.0, 300.0), 1.0, VIEW);
            assert_eq!(offset, (0.0, 0.0));
        }

        #[test]
        fn large_images_cannot_leave_the_viewport() {
            // 2000x1000 displayed in 800x600: 600 spare horizontally, 200 vertically.
            let offset = clamp_offset((9999.0, -9999.0), (2000.0, 1000.0), 1.0, VIEW);
            assert_eq!(offset, (600.0, -200.0));
        }

        #[test]
        fn only_the_overflowing_axis_pans() {
            let offset = clamp_offset((500.0, 500.0), (2000.0, 300.0), 1.0, VIEW);
            assert_eq!(offset, (500.0, 0.0));
        }

        #[test]
        fn zooming_at_the_centre_keeps_the_centre() {
            let offset =
                offset_after_zoom((0.0, 0.0), 1.0, 2.0, (400.0, 300.0), (4000.0, 3000.0), VIEW);
            assert_eq!(offset, (0.0, 0.0));
        }

        #[test]
        fn zooming_keeps_the_point_under_the_cursor() {
            let logical = (4000.0, 3000.0);
            let anchor = (600.0, 450.0);
            let offset = offset_after_zoom((0.0, 0.0), 1.0, 2.0, anchor, logical, VIEW);
            // The anchor is (200, 150) from centre; doubling zoom pushes the
            // image point under it to (400, 300) from centre, so the image
            // shifts by (-200, -150) to compensate.
            assert_eq!(offset, (-200.0, -150.0));
        }

        #[test]
        fn zooming_out_recentres_when_the_image_fits() {
            let offset = offset_after_zoom(
                (300.0, 200.0),
                2.0,
                0.1,
                (700.0, 100.0),
                (1000.0, 1000.0),
                VIEW,
            );
            assert_eq!(offset, (0.0, 0.0));
        }

        #[test]
        fn zoom_steps_snap_to_actual_size() {
            assert_eq!(step_zoom(0.9, 1.25), 1.0);
            assert_eq!(step_zoom(1.1, 0.8), 1.0);
            assert_eq!(step_zoom(1.0, 1.25), 1.25);
            assert_eq!(step_zoom(0.5, 1.25), 0.625);
        }

        #[test]
        fn zoom_steps_respect_limits() {
            assert_eq!(step_zoom(MAX_ZOOM, 1.25), MAX_ZOOM);
            assert_eq!(step_zoom(MIN_ZOOM, 0.8), MIN_ZOOM);
        }
    }
}

use geometry::{Grab, Selection, Viewport};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ZoomMode {
    /// Track the window: shrink to fit, never enlarge past 100%.
    Fit,
    Custom(f64),
}

/// A selection drag in progress: either one that holds part of the existing
/// selection, or a new selection grown from `start`.
#[derive(Clone, Copy)]
pub(crate) struct CropDrag {
    grab: Option<Grab>,
    from: Selection,
    start: (f64, f64),
}

mod imp {
    use super::*;
    use gtk::subclass::widget::WidgetImpl;

    pub struct Canvas {
        pub(super) texture: RefCell<Option<gdk::Texture>>,
        pub(super) frames: RefCell<Vec<(gdk::Texture, Duration)>>,
        pub(super) frame: Cell<usize>,
        pub(super) stream: RefCell<Option<FrameStream>>,
        pub(super) stream_delay: Cell<Duration>,
        pub(super) playing: Cell<bool>,
        pub(super) timer: RefCell<Option<glib::SourceId>>,
        pub(super) pixel_scale: Cell<f64>,
        pub(super) has_alpha: Cell<bool>,
        pub(super) mode: Cell<ZoomMode>,
        pub(super) quarter_turns: Cell<u8>,
        pub(super) offset: Cell<(f64, f64)>,
        pub(super) scroll_zooms: Cell<bool>,
        pub(super) checkerboard: Cell<bool>,
        pub(super) pointer: Cell<Option<(f64, f64)>>,
        pub(super) drag_origin: Cell<(f64, f64)>,
        pub(super) pinch_base: Cell<f64>,
        /// Present while cropping: the selection in displayed image pixels.
        pub(super) crop: Cell<Option<Selection>>,
        pub(super) crop_drag: Cell<Option<CropDrag>>,
        /// Textures replaced by crops, most recent last.
        pub(super) crop_history: RefCell<Vec<gdk::Texture>>,
        pub(super) listeners: RefCell<Vec<Rc<dyn Fn()>>>,
        pub(super) notify_queued: Cell<bool>,
    }

    impl Default for Canvas {
        fn default() -> Self {
            Self {
                texture: RefCell::new(None),
                frames: RefCell::new(Vec::new()),
                frame: Cell::new(0),
                stream: RefCell::new(None),
                stream_delay: Cell::new(Duration::ZERO),
                playing: Cell::new(false),
                timer: RefCell::new(None),
                pixel_scale: Cell::new(1.0),
                has_alpha: Cell::new(false),
                mode: Cell::new(ZoomMode::Fit),
                quarter_turns: Cell::new(0),
                offset: Cell::new((0.0, 0.0)),
                scroll_zooms: Cell::new(false),
                checkerboard: Cell::new(true),
                pointer: Cell::new(None),
                drag_origin: Cell::new((0.0, 0.0)),
                pinch_base: Cell::new(1.0),
                crop: Cell::new(None),
                crop_drag: Cell::new(None),
                crop_history: RefCell::new(Vec::new()),
                listeners: RefCell::new(Vec::new()),
                notify_queued: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "PreviewerCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("pv-canvas");
        }
    }

    impl ObjectImpl for Canvas {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_hexpand(true);
            obj.set_vexpand(true);
            obj.set_focusable(false);
            obj.set_overflow(gtk::Overflow::Hidden);

            // An animation has no business ticking while it is off screen.
            obj.connect_map(|canvas| canvas.schedule_frame());
            obj.connect_unmap(|canvas| canvas.cancel_timer());

            let drag = gtk::GestureDrag::new();
            drag.set_button(0);
            drag.connect_drag_begin(clone!(
                #[weak]
                obj,
                move |_, x, y| {
                    let imp = obj.imp();
                    imp.drag_origin.set(imp.offset.get());
                    if imp.crop.get().is_some() {
                        obj.begin_crop_drag(x, y);
                    } else if obj.can_pan() {
                        obj.set_cursor_from_name(Some("grabbing"));
                    }
                }
            ));
            drag.connect_drag_update(clone!(
                #[weak]
                obj,
                move |_, dx, dy| {
                    if obj.imp().crop.get().is_some() {
                        obj.update_crop_drag(dx, dy);
                        return;
                    }
                    let origin = obj.imp().drag_origin.get();
                    obj.set_offset((origin.0 + dx, origin.1 + dy));
                }
            ));
            drag.connect_drag_end(clone!(
                #[weak]
                obj,
                move |_, _, _| {
                    obj.imp().crop_drag.set(None);
                    obj.update_cursor();
                }
            ));
            obj.add_controller(drag);

            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(clone!(
                #[weak]
                obj,
                move |_, x, y| obj.imp().pointer.set(Some((x, y)))
            ));
            motion.connect_leave(clone!(
                #[weak]
                obj,
                move |_| obj.imp().pointer.set(None)
            ));
            obj.add_controller(motion);

            let scroll =
                gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            scroll.connect_scroll(clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |controller, dx, dy| {
                    if !obj.has_image() {
                        return glib::Propagation::Proceed;
                    }
                    let ctrl = controller
                        .current_event_state()
                        .contains(gdk::ModifierType::CONTROL_MASK);
                    if ctrl || obj.imp().scroll_zooms.get() {
                        let factor = (-dy * SCROLL_ZOOM_RATE).exp();
                        obj.set_zoom_at(obj.zoom() * factor, obj.imp().pointer.get());
                    } else {
                        let k = if controller.unit() == gdk::ScrollUnit::Wheel {
                            WHEEL_PAN_PIXELS
                        } else {
                            1.0
                        };
                        let (ox, oy) = obj.imp().offset.get();
                        obj.set_offset((ox - dx * k, oy - dy * k));
                    }
                    glib::Propagation::Stop
                }
            ));
            obj.add_controller(scroll);

            let pinch = gtk::GestureZoom::new();
            pinch.connect_begin(clone!(
                #[weak]
                obj,
                move |_, _| obj.imp().pinch_base.set(obj.zoom())
            ));
            pinch.connect_scale_changed(clone!(
                #[weak]
                obj,
                move |gesture, scale| {
                    obj.set_zoom_at(
                        obj.imp().pinch_base.get() * scale,
                        gesture.bounding_box_center(),
                    );
                }
            ));
            obj.add_controller(pinch);

            let click = gtk::GestureClick::new();
            click.set_button(gdk::BUTTON_PRIMARY);
            click.connect_pressed(clone!(
                #[weak]
                obj,
                move |_, presses, x, y| {
                    if presses == 2 && obj.imp().crop.get().is_none() {
                        obj.toggle_zoom_at((x, y));
                    }
                }
            ));
            obj.add_controller(click);

            theme::connect_changed(clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move || {
                    obj.queue_draw();
                    glib::ControlFlow::Continue
                }
            ));
        }
    }

    impl WidgetImpl for Canvas {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().schedule_notify();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let Some(logical) = obj.logical_size() else {
                return;
            };
            let view = obj.viewport();
            if view.width <= 0.0 || view.height <= 0.0 {
                return;
            }

            let zoom = obj.zoom();
            let css_per_logical = zoom / view.scale;
            let (disp_w, disp_h) = geometry::displayed(logical, zoom, view);
            let offset = geometry::clamp_offset(self.offset.get(), logical, zoom, view);
            self.offset.set(offset);

            // Land the image on whole device pixels so 100% stays sharp.
            let snap = |value: f64| (value * view.scale).round() / view.scale;
            let left = snap(view.width / 2.0 + offset.0 - disp_w / 2.0);
            let top = snap(view.height / 2.0 + offset.1 - disp_h / 2.0);

            if self.checkerboard.get() && self.has_alpha.get() {
                draw_checkerboard(
                    snapshot,
                    &graphene::Rect::new(left as f32, top as f32, disp_w as f32, disp_h as f32),
                );
            }

            let pixel_scale = self.pixel_scale.get();
            let device_per_texel = zoom / pixel_scale;
            let filter = if device_per_texel >= NEAREST_ABOVE {
                gtk::gsk::ScalingFilter::Nearest
            } else if device_per_texel < 1.0 {
                gtk::gsk::ScalingFilter::Trilinear
            } else {
                gtk::gsk::ScalingFilter::Linear
            };

            // Texture size on screen before rotation.
            let tex_w = (f64::from(texture.width()) / pixel_scale * css_per_logical) as f32;
            let tex_h = (f64::from(texture.height()) / pixel_scale * css_per_logical) as f32;

            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                (left + disp_w / 2.0) as f32,
                (top + disp_h / 2.0) as f32,
            ));
            snapshot.rotate(90.0 * f32::from(self.quarter_turns.get()));
            snapshot.append_scaled_texture(
                &texture,
                filter,
                &graphene::Rect::new(-tex_w / 2.0, -tex_h / 2.0, tex_w, tex_h),
            );
            snapshot.restore();

            if let Some(selection) = self.crop.get() {
                let to_widget = |x: f64, y: f64| {
                    (
                        (left + x * css_per_logical) as f32,
                        (top + y * css_per_logical) as f32,
                    )
                };
                draw_crop(
                    snapshot,
                    &graphene::Rect::new(left as f32, top as f32, disp_w as f32, disp_h as f32),
                    selection,
                    to_widget,
                );
            }
        }
    }

    /// Dims everything outside `selection`, outlines it and adds the handles.
    fn draw_crop(
        snapshot: &gtk::Snapshot,
        image: &graphene::Rect,
        selection: Selection,
        to_widget: impl Fn(f64, f64) -> (f32, f32),
    ) {
        let (x0, y0) = to_widget(selection.x0, selection.y0);
        let (x1, y1) = to_widget(selection.x1, selection.y1);
        let dim = gdk::RGBA::new(0.0, 0.0, 0.0, CROP_DIM);
        let (ix0, iy0) = (image.x(), image.y());
        let (ix1, iy1) = (ix0 + image.width(), iy0 + image.height());
        for (x, y, w, h) in [
            (ix0, iy0, image.width(), y0 - iy0),
            (ix0, y1, image.width(), iy1 - y1),
            (ix0, y0, x0 - ix0, y1 - y0),
            (x1, y0, ix1 - x1, y1 - y0),
        ] {
            if w > 0.0 && h > 0.0 {
                snapshot.append_color(&dim, &graphene::Rect::new(x, y, w, h));
            }
        }

        let white = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
        let black = gdk::RGBA::new(0.0, 0.0, 0.0, 0.6);
        let line = 1.0_f32;
        for (color, grow) in [(&black, line), (&white, 0.0)] {
            for (x, y, w, h) in [
                (x0 - grow, y0 - grow, x1 - x0 + 2.0 * grow, line),
                (x0 - grow, y1 - line + grow, x1 - x0 + 2.0 * grow, line),
                (x0 - grow, y0 - grow, line, y1 - y0 + 2.0 * grow),
                (x1 - line + grow, y0 - grow, line, y1 - y0 + 2.0 * grow),
            ] {
                snapshot.append_color(color, &graphene::Rect::new(x, y, w, h));
            }
        }

        let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let half = CROP_HANDLE / 2.0;
        for (hx, hy) in [
            (x0, y0),
            (mx, y0),
            (x1, y0),
            (x0, my),
            (x1, my),
            (x0, y1),
            (mx, y1),
            (x1, y1),
        ] {
            snapshot.append_color(
                &black,
                &graphene::Rect::new(
                    hx - half - 1.0,
                    hy - half - 1.0,
                    CROP_HANDLE + 2.0,
                    CROP_HANDLE + 2.0,
                ),
            );
            snapshot.append_color(
                &white,
                &graphene::Rect::new(hx - half, hy - half, CROP_HANDLE, CROP_HANDLE),
            );
        }
    }

    fn draw_checkerboard(snapshot: &gtk::Snapshot, bounds: &graphene::Rect) {
        let (light, dark) = theme::checker_colors();
        snapshot.push_clip(bounds);
        snapshot.append_color(&light, bounds);
        let tile = graphene::Rect::new(
            bounds.x(),
            bounds.y(),
            CHECKER_SQUARE * 2.0,
            CHECKER_SQUARE * 2.0,
        );
        snapshot.push_repeat(bounds, Some(&tile));
        snapshot.append_color(
            &dark,
            &graphene::Rect::new(bounds.x(), bounds.y(), CHECKER_SQUARE, CHECKER_SQUARE),
        );
        snapshot.append_color(
            &dark,
            &graphene::Rect::new(
                bounds.x() + CHECKER_SQUARE,
                bounds.y() + CHECKER_SQUARE,
                CHECKER_SQUARE,
                CHECKER_SQUARE,
            ),
        );
        snapshot.pop();
        snapshot.pop();
    }
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Shows `texture` (or nothing), resetting zoom, pan and rotation.
    pub fn set_image(&self, texture: Option<gdk::Texture>, pixel_scale: f64, has_alpha: bool) {
        let imp = self.imp();
        self.cancel_timer();
        imp.crop.set(None);
        imp.crop_drag.set(None);
        imp.crop_history.borrow_mut().clear();
        imp.frames.borrow_mut().clear();
        imp.stream.take();
        imp.frame.set(0);
        imp.playing.set(false);
        *imp.texture.borrow_mut() = texture;
        imp.pixel_scale
            .set(if pixel_scale > 0.0 { pixel_scale } else { 1.0 });
        imp.has_alpha.set(has_alpha);
        imp.mode.set(ZoomMode::Fit);
        imp.quarter_turns.set(0);
        imp.offset.set((0.0, 0.0));
        self.notify_view();
    }

    /// Shows an animation, looping until another image replaces it. With
    /// `autoplay` false it waits on its first frame for [`Self::toggle_playback`].
    pub fn set_animation(
        &self,
        frames: Vec<(gdk::Texture, Duration)>,
        pixel_scale: f64,
        has_alpha: bool,
        autoplay: bool,
    ) {
        let Some((first, _)) = frames.first().cloned() else {
            self.set_image(None, pixel_scale, has_alpha);
            return;
        };
        self.set_image(Some(first), pixel_scale, has_alpha);
        let imp = self.imp();
        let animated = frames.len() > 1;
        *imp.frames.borrow_mut() = frames;
        imp.playing.set(animated && autoplay);
        self.schedule_frame();
        self.notify_view();
    }

    /// Like [`Self::set_animation`] for an animation decoded on the fly:
    /// `first` shows for `delay`, then frames come from `stream`.
    pub fn set_stream(
        &self,
        first: gdk::Texture,
        delay: Duration,
        stream: FrameStream,
        pixel_scale: f64,
        has_alpha: bool,
        autoplay: bool,
    ) {
        self.set_image(Some(first), pixel_scale, has_alpha);
        let imp = self.imp();
        *imp.stream.borrow_mut() = Some(stream);
        imp.stream_delay.set(delay);
        imp.playing.set(autoplay);
        self.schedule_frame();
        self.notify_view();
    }

    pub fn is_animated(&self) -> bool {
        self.imp().frames.borrow().len() > 1 || self.imp().stream.borrow().is_some()
    }

    pub fn frame_count(&self) -> usize {
        self.imp().frames.borrow().len()
    }

    pub fn is_playing(&self) -> bool {
        self.imp().playing.get()
    }

    pub fn toggle_playback(&self) {
        if !self.is_animated() {
            return;
        }
        let imp = self.imp();
        imp.playing.set(!imp.playing.get());
        self.schedule_frame();
        self.notify_view();
    }

    /// Arms the timer for the current frame's delay, if the animation is
    /// playing and on screen.
    fn schedule_frame(&self) {
        self.cancel_timer();
        let imp = self.imp();
        if !imp.playing.get() || !self.is_mapped() {
            return;
        }
        let delay = if imp.stream.borrow().is_some() {
            imp.stream_delay.get()
        } else {
            match imp.frames.borrow().get(imp.frame.get()) {
                Some((_, delay)) => *delay,
                None => return,
            }
        };
        self.arm_timer(delay);
    }

    fn arm_timer(&self, delay: Duration) {
        let imp = self.imp();
        let id = glib::timeout_add_local_once(
            delay,
            clone!(
                #[weak(rename_to = canvas)]
                self,
                move || canvas.advance_frame()
            ),
        );
        *imp.timer.borrow_mut() = Some(id);
    }

    fn cancel_timer(&self) {
        if let Some(id) = self.imp().timer.take() {
            id.remove();
        }
    }

    fn advance_frame(&self) {
        let imp = self.imp();
        // This source has fired; forget it so it is never removed twice.
        imp.timer.take();
        if imp.stream.borrow().is_some() {
            self.advance_stream();
            return;
        }
        let next = {
            let frames = imp.frames.borrow();
            if frames.is_empty() {
                return;
            }
            let next = (imp.frame.get() + 1) % frames.len();
            *imp.texture.borrow_mut() = Some(frames[next].0.clone());
            next
        };
        imp.frame.set(next);
        self.queue_draw();
        self.schedule_frame();
    }

    fn advance_stream(&self) {
        let imp = self.imp();
        let next = match imp.stream.borrow().as_ref().map(FrameStream::next) {
            Some(Ok(next)) => next,
            _ => {
                // The decoder gave up; leave the last frame up.
                imp.stream.take();
                imp.playing.set(false);
                self.notify_view();
                return;
            }
        };
        let Some(frame) = next else {
            // Decoding is behind; look again shortly instead of skipping.
            self.arm_timer(Duration::from_millis(8));
            return;
        };
        let (width, height) = {
            let texture = imp.texture.borrow();
            let Some(texture) = texture.as_ref() else {
                return;
            };
            (texture.width(), texture.height())
        };
        let texture = gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(frame.rgba),
            width as usize * 4,
        );
        *imp.texture.borrow_mut() = Some(texture.upcast());
        imp.stream_delay.set(frame.delay);
        self.queue_draw();
        self.schedule_frame();
    }

    pub fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }

    pub fn has_image(&self) -> bool {
        self.imp().texture.borrow().is_some()
    }

    pub fn set_scroll_zooms(&self, value: bool) {
        self.imp().scroll_zooms.set(value);
    }

    pub fn set_checkerboard(&self, value: bool) {
        self.imp().checkerboard.set(value);
        self.queue_draw();
    }

    /// Current zoom as a percentage, `None` without an image.
    pub fn zoom_percent(&self) -> Option<f64> {
        self.has_image().then(|| self.zoom() * 100.0)
    }

    /// Runs `listener` whenever zoom, rotation or size changes.
    pub fn connect_view_changed(&self, listener: impl Fn() + 'static) {
        self.imp().listeners.borrow_mut().push(Rc::new(listener));
    }

    pub fn zoom_in(&self) {
        self.set_zoom_at(geometry::step_zoom(self.zoom(), ZOOM_STEP), None);
    }

    pub fn zoom_out(&self) {
        self.set_zoom_at(geometry::step_zoom(self.zoom(), 1.0 / ZOOM_STEP), None);
    }

    pub fn zoom_fit(&self) {
        if !self.has_image() {
            return;
        }
        let imp = self.imp();
        imp.mode.set(ZoomMode::Fit);
        imp.offset.set((0.0, 0.0));
        self.notify_view();
    }

    pub fn zoom_actual(&self) {
        self.set_zoom_at(1.0, None);
    }

    pub fn rotate(&self, clockwise: bool) {
        if !self.has_image() {
            return;
        }
        let imp = self.imp();
        imp.crop.set(None);
        let turns = imp.quarter_turns.get();
        imp.quarter_turns
            .set((turns + if clockwise { 1 } else { 3 }) % 4);
        imp.offset.set((0.0, 0.0));
        self.notify_view();
    }

    fn viewport(&self) -> Viewport {
        // The surface knows fractional scales (1.5x, ...); `scale_factor` only
        // reports the integer ceiling, which would make "100%" wrong.
        let scale = self
            .native()
            .and_then(|native| native.surface())
            .map(|surface| surface.scale())
            .filter(|scale| *scale >= 1.0)
            .unwrap_or_else(|| f64::from(self.scale_factor().max(1)));
        Viewport {
            width: f64::from(self.width()),
            height: f64::from(self.height()),
            scale,
        }
    }

    /// Image size in logical pixels as displayed, i.e. after rotation.
    fn logical_size(&self) -> Option<(f64, f64)> {
        let imp = self.imp();
        let texture = imp.texture.borrow();
        let texture = texture.as_ref()?;
        let scale = imp.pixel_scale.get();
        let (w, h) = (
            f64::from(texture.width()) / scale,
            f64::from(texture.height()) / scale,
        );
        Some(if imp.quarter_turns.get() % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        })
    }

    fn zoom(&self) -> f64 {
        match self.imp().mode.get() {
            ZoomMode::Custom(zoom) => zoom,
            ZoomMode::Fit => match self.logical_size() {
                Some(logical) => geometry::fit_zoom(logical, self.viewport()),
                None => 1.0,
            },
        }
    }

    fn can_pan(&self) -> bool {
        let Some(logical) = self.logical_size() else {
            return false;
        };
        let view = self.viewport();
        let (w, h) = geometry::displayed(logical, self.zoom(), view);
        w > view.width + 0.5 || h > view.height + 0.5
    }

    fn set_offset(&self, offset: (f64, f64)) {
        let Some(logical) = self.logical_size() else {
            return;
        };
        let clamped = geometry::clamp_offset(offset, logical, self.zoom(), self.viewport());
        self.imp().offset.set(clamped);
        self.queue_draw();
    }

    /// Zooms to `zoom`, keeping the image point under `anchor` (widget
    /// coordinates; the centre when `None`) where it is.
    fn set_zoom_at(&self, zoom: f64, anchor: Option<(f64, f64)>) {
        let Some(logical) = self.logical_size() else {
            return;
        };
        let view = self.viewport();
        let old = self.zoom();
        let new = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let anchor = anchor.unwrap_or((view.width / 2.0, view.height / 2.0));
        let imp = self.imp();
        let offset = geometry::offset_after_zoom(imp.offset.get(), old, new, anchor, logical, view);
        imp.mode.set(ZoomMode::Custom(new));
        imp.offset.set(offset);
        self.notify_view();
    }

    /// Double-click: 100% around the pointer, or back to fit.
    fn toggle_zoom_at(&self, anchor: (f64, f64)) {
        let Some(logical) = self.logical_size() else {
            return;
        };
        match self.imp().mode.get() {
            ZoomMode::Fit => {
                let fit = geometry::fit_zoom(logical, self.viewport());
                let target = if fit < 1.0 { 1.0 } else { 2.0 };
                self.set_zoom_at(target, Some(anchor));
            }
            ZoomMode::Custom(_) => self.zoom_fit(),
        }
    }

    fn update_cursor(&self) {
        let name = if self.imp().crop.get().is_some() {
            Some("crosshair")
        } else if self.can_pan() {
            Some("grab")
        } else {
            None
        };
        self.set_cursor_from_name(name);
    }

    // ---- cropping -------------------------------------------------------

    /// Whether the selection can be started: a still image is showing.
    pub fn can_crop(&self) -> bool {
        self.has_image() && !self.is_animated()
    }

    pub fn is_cropping(&self) -> bool {
        self.imp().crop.get().is_some()
    }

    /// Shows a selection covering the whole image to drag in.
    pub fn begin_crop(&self) {
        if !self.can_crop() {
            return;
        }
        let Some(logical) = self.logical_size() else {
            return;
        };
        let imp = self.imp();
        imp.crop.set(Some(Selection::full(logical)));
        imp.crop_drag.set(None);
        self.notify_view();
    }

    pub fn cancel_crop(&self) {
        let imp = self.imp();
        if imp.crop.take().is_some() {
            imp.crop_drag.set(None);
            self.notify_view();
        }
    }

    /// Image size in logical pixels of the selection, for the status bar.
    pub fn crop_size(&self) -> Option<(u32, u32)> {
        let selection = self.imp().crop.get()?;
        Some((
            selection.width().round().max(1.0) as u32,
            selection.height().round().max(1.0) as u32,
        ))
    }

    /// Replaces the image with the selected part. Returns the new size in
    /// logical pixels, or `None` if there was nothing to crop.
    pub fn apply_crop(&self) -> Option<(u32, u32)> {
        let imp = self.imp();
        let selection = imp.crop.take()?;
        imp.crop_drag.set(None);
        let texture = self.texture()?;
        let scale = imp.pixel_scale.get();
        let texels = Selection {
            x0: selection.x0 * scale,
            y0: selection.y0 * scale,
            x1: selection.x1 * scale,
            y1: selection.y1 * scale,
        };
        let (x, y, w, h) = geometry::texture_rect(
            texels,
            imp.quarter_turns.get(),
            (texture.width() as u32, texture.height() as u32),
        );

        let mut downloader = gdk::TextureDownloader::new(&texture);
        downloader.set_format(gdk::MemoryFormat::R8g8b8a8Premultiplied);
        let (bytes, stride) = downloader.download_bytes();
        let row = w as usize * 4;
        let mut pixels = Vec::with_capacity(row * h as usize);
        for line in y..y + h {
            let start = line as usize * stride + x as usize * 4;
            pixels.extend_from_slice(&bytes[start..start + row]);
        }
        let cropped = gdk::MemoryTexture::new(
            w as i32,
            h as i32,
            gdk::MemoryFormat::R8g8b8a8Premultiplied,
            &glib::Bytes::from_owned(pixels),
            row,
        );

        imp.crop_history.borrow_mut().push(texture);
        *imp.texture.borrow_mut() = Some(cropped.upcast());
        imp.mode.set(ZoomMode::Fit);
        imp.offset.set((0.0, 0.0));
        self.notify_view();
        Some((
            (f64::from(w) / scale).round().max(1.0) as u32,
            (f64::from(h) / scale).round().max(1.0) as u32,
        ))
    }

    pub fn can_undo_crop(&self) -> bool {
        !self.imp().crop_history.borrow().is_empty()
    }

    /// Brings back the image from before the last crop. Returns its size in
    /// logical pixels, or `None` if there was no crop to undo.
    pub fn undo_crop(&self) -> Option<(u32, u32)> {
        let imp = self.imp();
        let texture = imp.crop_history.borrow_mut().pop()?;
        imp.crop.set(None);
        imp.crop_drag.set(None);
        let scale = imp.pixel_scale.get();
        let size = (
            (f64::from(texture.width()) / scale).round().max(1.0) as u32,
            (f64::from(texture.height()) / scale).round().max(1.0) as u32,
        );
        *imp.texture.borrow_mut() = Some(texture);
        imp.mode.set(ZoomMode::Fit);
        imp.offset.set((0.0, 0.0));
        self.notify_view();
        Some(size)
    }

    /// A widget point as a position in the displayed image, in logical
    /// pixels. It may lie outside the image.
    fn widget_to_image(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let logical = self.logical_size()?;
        let view = self.viewport();
        let zoom = self.zoom();
        let css_per_logical = zoom / view.scale;
        let (dw, dh) = geometry::displayed(logical, zoom, view);
        let offset = self.imp().offset.get();
        let left = view.width / 2.0 + offset.0 - dw / 2.0;
        let top = view.height / 2.0 + offset.1 - dh / 2.0;
        Some(((x - left) / css_per_logical, (y - top) / css_per_logical))
    }

    fn css_per_logical(&self) -> f64 {
        self.zoom() / self.viewport().scale
    }

    fn begin_crop_drag(&self, x: f64, y: f64) {
        let imp = self.imp();
        let (Some(selection), Some(start)) = (imp.crop.get(), self.widget_to_image(x, y)) else {
            return;
        };
        let tolerance = CROP_GRAB / self.css_per_logical();
        imp.crop_drag.set(Some(CropDrag {
            grab: selection.hit(start, tolerance),
            from: selection,
            start,
        }));
    }

    fn update_crop_drag(&self, dx: f64, dy: f64) {
        let imp = self.imp();
        let (Some(drag), Some(bounds)) = (imp.crop_drag.get(), self.logical_size()) else {
            return;
        };
        let per = self.css_per_logical();
        let delta = (dx / per, dy / per);
        let selection = match drag.grab {
            Some(grab) => drag.from.dragged(grab, delta, bounds),
            None => Selection::between(
                drag.start,
                (drag.start.0 + delta.0, drag.start.1 + delta.1),
                bounds,
            ),
        };
        imp.crop.set(Some(selection));
        self.notify_view();
    }

    /// Redraws now and tells listeners on the next idle, which keeps their
    /// work (like updating a label) out of the allocation pass.
    fn notify_view(&self) {
        self.queue_draw();
        self.schedule_notify();
    }

    fn schedule_notify(&self) {
        let imp = self.imp();
        if imp.notify_queued.replace(true) {
            return;
        }
        glib::idle_add_local_once(clone!(
            #[weak(rename_to = canvas)]
            self,
            move || {
                canvas.imp().notify_queued.set(false);
                canvas.update_cursor();
                let listeners: Vec<Rc<dyn Fn()>> = canvas.imp().listeners.borrow().clone();
                for listener in listeners {
                    listener();
                }
            }
        ));
    }
}
