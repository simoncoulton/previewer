// SPDX-License-Identifier: MIT

//! Turning files into pixels.
//!
//! Decoding runs on worker threads and produces a plain [`RawImage`]; the
//! GTK texture is built on the main thread. Formats the built-in decoders do
//! not know (HEIC, AVIF, JPEG XL, ...) are left to GDK's own loaders by the
//! caller, see [`load_with_gdk`].

use std::{
    fs::File,
    io::BufReader,
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use gtk::{gdk, gio, glib, prelude::*};
use image::{AnimationDecoder as _, ImageDecoder as _};
use resvg::{tiny_skia, usvg};

/// Longest side, in pixels, an SVG is rasterised to when it is small.
const SVG_MIN_RASTER: f32 = 2048.0;
/// Hard cap on the rasterised side of an SVG.
const SVG_MAX_RASTER: f32 = 8192.0;
/// Animations needing more pixel memory than this are streamed, not held.
const ANIMATION_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Browsers treat tiny GIF delays as "unspecified" and play at 10 fps; so do we.
const MIN_FRAME_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_FRAME_DELAY: Duration = Duration::from_millis(100);

pub struct RawImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub premultiplied: bool,
    /// Whether any pixel is less than fully opaque.
    pub has_alpha: bool,
    /// Texture pixels per image pixel. 1.0 for photos; above 1.0 for vector
    /// images rasterised larger than their nominal size.
    pub pixel_scale: f64,
    pub format: String,
    /// Every frame of an animation (the first is also `rgba`); empty for a
    /// still image. Frames share the dimensions and `premultiplied` of `self`.
    pub animation: Vec<AnimFrame>,
    /// Set instead of `animation` for animations too large to hold: how long
    /// the first frame (`rgba`) shows, and the decoder for the rest.
    pub stream: Option<(Duration, FrameStream)>,
}

pub struct AnimFrame {
    pub rgba: Vec<u8>,
    pub delay: Duration,
}

impl RawImage {
    pub fn into_texture(self) -> gdk::Texture {
        let format = if self.premultiplied {
            gdk::MemoryFormat::R8g8b8a8Premultiplied
        } else {
            gdk::MemoryFormat::R8g8b8a8
        };
        let stride = self.width as usize * 4;
        gdk::MemoryTexture::new(
            self.width as i32,
            self.height as i32,
            format,
            &glib::Bytes::from_owned(self.rgba),
            stride,
        )
        .upcast()
    }

    /// Textures for each animation frame with how long to show it.
    pub fn frame_textures(&self) -> Vec<(gdk::Texture, Duration)> {
        let format = if self.premultiplied {
            gdk::MemoryFormat::R8g8b8a8Premultiplied
        } else {
            gdk::MemoryFormat::R8g8b8a8
        };
        self.animation
            .iter()
            .map(|frame| {
                let texture = gdk::MemoryTexture::new(
                    self.width as i32,
                    self.height as i32,
                    format,
                    &glib::Bytes::from(&frame.rgba[..]),
                    self.width as usize * 4,
                );
                (texture.upcast(), frame.delay)
            })
            .collect()
    }
}

pub enum DecodeError {
    /// None of the built-in decoders recognise the file.
    Unsupported,
    Failed(String),
}

fn any_transparency(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 255)
}

fn is_svg(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"))
}

/// Decodes a file at full size.
pub fn decode(path: &Path) -> Result<RawImage, DecodeError> {
    if is_svg(path) {
        decode_svg(path, None)
    } else {
        decode_raster(path, true)
    }
}

/// Decodes a file and shrinks it so neither side exceeds `size`.
pub fn thumbnail(path: &Path, size: u32) -> Result<RawImage, DecodeError> {
    if is_svg(path) {
        return decode_svg(path, Some(size as f32));
    }
    let mut raw = decode_raster(path, false)?;
    if raw.width <= size && raw.height <= size {
        return Ok(raw);
    }
    let Some(buffer) =
        image::RgbaImage::from_raw(raw.width, raw.height, std::mem::take(&mut raw.rgba))
    else {
        return Err(DecodeError::Failed("corrupt pixel buffer".to_owned()));
    };
    // `imageops::thumbnail` resizes to exactly the size asked for, so work out
    // the aspect-correct target first.
    let scale = f64::from(size) / f64::from(raw.width.max(raw.height));
    let width = ((f64::from(raw.width) * scale).round() as u32).max(1);
    let height = ((f64::from(raw.height) * scale).round() as u32).max(1);
    let resized = image::imageops::thumbnail(&buffer, width, height);
    let rgba = resized.into_raw();
    Ok(RawImage {
        width,
        height,
        has_alpha: any_transparency(&rgba),
        rgba,
        premultiplied: false,
        pixel_scale: 1.0,
        format: raw.format,
        animation: Vec::new(),
        stream: None,
    })
}

/// How an animation is delivered.
enum Animation {
    /// Small enough to keep every decoded frame; looping costs nothing.
    Frames(u32, u32, Vec<AnimFrame>),
    /// Too big to hold: the first frame, and a thread that decodes the rest
    /// ahead of playback and starts over at the end.
    Stream(u32, u32, AnimFrame, FrameStream),
}

/// Frames decoded ahead of the one on screen.
const STREAM_LOOKAHEAD: usize = 6;

/// A background decoder that loops a large animation forever. Dropping it
/// stops the thread.
pub struct FrameStream {
    receiver: std::sync::mpsc::Receiver<AnimFrame>,
}

impl FrameStream {
    fn start(path: std::path::PathBuf, format: image::ImageFormat) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(STREAM_LOOKAHEAD);
        std::thread::spawn(move || {
            // The first frame is already on screen when the stream starts.
            let mut skip = 1;
            loop {
                let Some(frames) = open_frames(&path, format) else {
                    return;
                };
                let mut sent = 0;
                for frame in frames.skip(skip) {
                    let Ok(frame) = frame else { break };
                    let delay = frame_delay(frame.delay());
                    let rgba = frame.into_buffer().into_raw();
                    if sender.send(AnimFrame { rgba, delay }).is_err() {
                        return;
                    }
                    sent += 1;
                }
                if sent == 0 && skip == 0 {
                    return; // unreadable now; do not spin
                }
                skip = 0;
            }
        });
        Self { receiver }
    }

    /// The next frame if the decoder has one ready. `Err` means it stopped.
    pub fn next(&self) -> Result<Option<AnimFrame>, ()> {
        use std::sync::mpsc::TryRecvError;
        match self.receiver.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()),
        }
    }
}

/// The frames of a GIF, animated PNG or animated WebP, composited to the
/// full canvas. `None` for stills.
fn open_frames(path: &Path, format: image::ImageFormat) -> Option<image::Frames<'static>> {
    use image::{
        ImageFormat,
        codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    };

    let open = || File::open(path).ok().map(BufReader::new);
    Some(match format {
        ImageFormat::Gif => GifDecoder::new(open()?).ok()?.into_frames(),
        ImageFormat::Png => {
            let decoder = PngDecoder::new(open()?).ok()?;
            if !decoder.is_apng().ok()? {
                return None;
            }
            decoder.apng().ok()?.into_frames()
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(open()?).ok()?;
            if !decoder.has_animation() {
                return None;
            }
            decoder.into_frames()
        }
        _ => return None,
    })
}

/// Decodes an animation, or `None` for a still (or a failure, in which case
/// the still path reports the real error).
fn decode_animation(path: &Path, format: image::ImageFormat) -> Option<Animation> {
    let mut frames = open_frames(path, format)?;
    let mut size = None;
    let mut total = 0usize;
    let mut out: Vec<AnimFrame> = Vec::new();
    let mut over_budget = false;
    for frame in frames.by_ref() {
        let frame = frame.ok()?;
        let delay = frame_delay(frame.delay());
        let buffer = frame.into_buffer();
        let dims = buffer.dimensions();
        if *size.get_or_insert(dims) != dims {
            return None;
        }
        total += buffer.as_raw().len();
        out.push(AnimFrame {
            rgba: buffer.into_raw(),
            delay,
        });
        if total > ANIMATION_MAX_BYTES {
            over_budget = true;
            break;
        }
    }
    let (width, height) = size?;
    if over_budget {
        let first = out.swap_remove(0);
        drop(out);
        return Some(Animation::Stream(
            width,
            height,
            first,
            FrameStream::start(path.to_path_buf(), format),
        ));
    }
    (out.len() > 1).then_some(Animation::Frames(width, height, out))
}

fn frame_delay(delay: image::Delay) -> Duration {
    let (numer, denom) = delay.numer_denom_ms();
    let millis = f64::from(numer) / f64::from(denom.max(1));
    let delay = Duration::from_secs_f64(millis / 1000.0);
    if delay < MIN_FRAME_DELAY {
        DEFAULT_FRAME_DELAY
    } else {
        delay
    }
}

fn decode_raster(path: &Path, animated: bool) -> Result<RawImage, DecodeError> {
    let reader = image::ImageReader::open(path)
        .map_err(|error| DecodeError::Failed(error.to_string()))?
        .with_guessed_format()
        .map_err(|error| DecodeError::Failed(error.to_string()))?;
    let Some(format) = reader.format() else {
        return Err(DecodeError::Unsupported);
    };
    let name = format
        .extensions_str()
        .first()
        .map_or_else(|| format!("{format:?}"), |ext| ext.to_uppercase());

    match animated.then(|| decode_animation(path, format)).flatten() {
        Some(Animation::Frames(width, height, animation)) => {
            return Ok(RawImage {
                width,
                height,
                rgba: animation[0].rgba.clone(),
                premultiplied: false,
                has_alpha: animation.iter().any(|f| any_transparency(&f.rgba)),
                pixel_scale: 1.0,
                format: name,
                animation,
                stream: None,
            });
        }
        Some(Animation::Stream(width, height, first, stream)) => {
            let delay = first.delay;
            return Ok(RawImage {
                width,
                height,
                has_alpha: any_transparency(&first.rgba),
                rgba: first.rgba,
                premultiplied: false,
                pixel_scale: 1.0,
                format: name,
                animation: Vec::new(),
                stream: Some((delay, stream)),
            });
        }
        None => {}
    }

    let mut decoder = reader.into_decoder().map_err(|error| match error {
        image::ImageError::Unsupported(_) => DecodeError::Unsupported,
        other => DecodeError::Failed(other.to_string()),
    })?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded = image::DynamicImage::from_decoder(decoder).map_err(|error| match error {
        image::ImageError::Unsupported(_) => DecodeError::Unsupported,
        other => DecodeError::Failed(other.to_string()),
    })?;
    decoded.apply_orientation(orientation);

    let rgba = decoded.into_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let rgba = rgba.into_raw();
    Ok(RawImage {
        width,
        height,
        has_alpha: any_transparency(&rgba),
        rgba,
        premultiplied: false,
        pixel_scale: 1.0,
        format: name,
        animation: Vec::new(),
        stream: None,
    })
}

fn system_fonts() -> Arc<usvg::fontdb::Database> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut database = usvg::fontdb::Database::new();
            database.load_system_fonts();
            Arc::new(database)
        })
        .clone()
}

/// Rasterises an SVG. With `fit`, the longest side is exactly that many
/// pixels; without it, small drawings are rendered large enough to stay crisp
/// when zoomed.
fn decode_svg(path: &Path, fit: Option<f32>) -> Result<RawImage, DecodeError> {
    let data = std::fs::read(path).map_err(|error| DecodeError::Failed(error.to_string()))?;
    let options = usvg::Options {
        resources_dir: path.parent().map(Path::to_path_buf),
        fontdb: system_fonts(),
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(&data, &options)
        .map_err(|error| DecodeError::Failed(format!("invalid SVG: {error}")))?;

    let size = tree.size();
    let (nominal_w, nominal_h) = (size.width(), size.height());
    let longest = nominal_w.max(nominal_h);
    if !(longest.is_finite() && longest > 0.0) {
        return Err(DecodeError::Failed("the SVG has no size".to_owned()));
    }
    let target = match fit {
        Some(side) => side,
        None => longest.clamp(SVG_MIN_RASTER, SVG_MAX_RASTER),
    };
    let scale = target / longest;
    let width = ((nominal_w * scale).round() as u32).max(1);
    let height = ((nominal_h * scale).round() as u32).max(1);

    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| DecodeError::Failed("the SVG is too large to render".to_owned()))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(width as f32 / nominal_w, height as f32 / nominal_h),
        &mut pixmap.as_mut(),
    );

    let rgba = pixmap.take();
    Ok(RawImage {
        width,
        height,
        has_alpha: any_transparency(&rgba),
        rgba,
        premultiplied: true,
        pixel_scale: f64::from(width) / f64::from(nominal_w),
        format: "SVG".to_owned(),
        animation: Vec::new(),
        stream: None,
    })
}

/// Last resort for formats the built-in decoders lack: whatever GDK can load
/// on this system (HEIC, AVIF, JPEG XL, ... through gdk-pixbuf/glycin).
/// Blocks, so call it from the main thread only for rare formats.
pub fn load_with_gdk(path: &Path) -> Result<(gdk::Texture, String), String> {
    let file = gio::File::for_path(path);
    let texture = gdk::Texture::from_file(&file).map_err(|error| error.to_string())?;
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(|| "Image".to_owned(), str::to_uppercase);
    Ok((texture, format))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("previewer-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
        img.save(path).unwrap();
    }

    #[test]
    fn decodes_png() {
        let path = scratch("a.png");
        write_png(&path, 40, 20);
        let raw = decode(&path).ok().unwrap();
        assert_eq!((raw.width, raw.height), (40, 20));
        assert_eq!(raw.rgba.len(), 40 * 20 * 4);
        assert_eq!(raw.format, "PNG");
        assert!(!raw.premultiplied);
    }

    /// A 40x20 JPEG, optionally tagged with an EXIF orientation.
    fn write_jpeg(path: &Path, orientation: Option<u16>) {
        let img =
            image::RgbImage::from_fn(40, 20, |x, y| image::Rgb([x as u8 * 6, y as u8 * 12, 90]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode_image(&img)
            .unwrap();
        if let Some(value) = orientation {
            let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend_from_slice(&value.to_le_bytes());
            exif.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            let length = (exif.len() + 2) as u16;
            let mut segment = vec![0xFF, 0xE1];
            segment.extend_from_slice(&length.to_be_bytes());
            segment.extend_from_slice(&exif);
            // Right after the SOI marker.
            bytes.splice(2..2, segment);
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn decodes_jpeg() {
        let path = scratch("plain.jpg");
        write_jpeg(&path, None);
        let raw = decode(&path).ok().unwrap();
        assert_eq!((raw.width, raw.height), (40, 20));
        assert!(!raw.has_alpha);
    }

    #[test]
    fn exif_orientation_is_applied() {
        let path = scratch("rotated.jpg");
        write_jpeg(&path, Some(6));
        let raw = decode(&path).ok().unwrap();
        assert_eq!((raw.width, raw.height), (20, 40), "rotated a quarter turn");
    }

    #[test]
    fn transparency_is_detected() {
        let path = scratch("see-through.png");
        image::RgbaImage::from_fn(4, 4, |x, _| {
            image::Rgba([1, 2, 3, if x == 0 { 10 } else { 255 }])
        })
        .save(&path)
        .unwrap();
        assert!(decode(&path).ok().unwrap().has_alpha);
    }

    #[test]
    fn sniffs_content_not_extension() {
        let path = scratch("really-a-png.jpg");
        write_png(&path.with_extension("png"), 8, 8);
        fs::rename(path.with_extension("png"), &path).unwrap();
        let raw = decode(&path).ok().unwrap();
        assert_eq!(raw.format, "PNG");
    }

    #[test]
    fn unknown_content_is_unsupported() {
        let path = scratch("junk.bin");
        fs::write(&path, b"definitely not an image").unwrap();
        assert!(matches!(decode(&path), Err(DecodeError::Unsupported)));
    }

    #[test]
    fn missing_file_fails() {
        assert!(matches!(
            decode(Path::new("/nonexistent/x.png")),
            Err(DecodeError::Failed(_))
        ));
    }

    #[test]
    fn thumbnails_keep_aspect_ratio() {
        let path = scratch("wide.png");
        write_png(&path, 400, 100);
        let thumb = thumbnail(&path, 100).ok().unwrap();
        assert_eq!((thumb.width, thumb.height), (100, 25));
        assert_eq!(thumb.rgba.len(), 100 * 25 * 4);
    }

    #[test]
    fn thumbnails_never_enlarge() {
        let path = scratch("tiny.png");
        write_png(&path, 10, 10);
        let thumb = thumbnail(&path, 100).ok().unwrap();
        assert_eq!((thumb.width, thumb.height), (10, 10));
    }

    #[test]
    fn rasterises_small_svg_large() {
        let path = scratch("icon.svg");
        fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="8"><rect width="16" height="8" fill="#ff0000"/></svg>"##,
        )
        .unwrap();
        let raw = decode(&path).ok().unwrap();
        assert_eq!(raw.width, 2048);
        assert_eq!(raw.height, 1024);
        assert!((raw.pixel_scale - 128.0).abs() < 1e-6);
        assert!(raw.premultiplied);
        // Opaque red in the middle.
        let mid = ((512 * 2048 + 1024) * 4) as usize;
        assert_eq!(&raw.rgba[mid..mid + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn svg_thumbnail_fits_the_box() {
        let path = scratch("thumb.svg");
        fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="100"><rect width="400" height="100" fill="#00f"/></svg>"##,
        )
        .unwrap();
        let thumb = thumbnail(&path, 100).ok().unwrap();
        assert_eq!((thumb.width, thumb.height), (100, 25));
    }

    #[test]
    fn broken_svg_reports_failure() {
        let path = scratch("broken.svg");
        fs::write(&path, "<svg").unwrap();
        assert!(matches!(decode(&path), Err(DecodeError::Failed(_))));
    }

    fn write_gif(path: &Path, delays_ms: &[u32]) {
        use image::{Delay, Frame, RgbaImage, codecs::gif::GifEncoder};
        let mut encoder = GifEncoder::new(File::create(path).unwrap());
        for (n, ms) in delays_ms.iter().enumerate() {
            let shade = 60 * (n as u8 + 1);
            let pixels = RgbaImage::from_pixel(8, 6, image::Rgba([shade, 0, 0, 255]));
            let frame = Frame::from_parts(pixels, 0, 0, Delay::from_numer_denom_ms(*ms, 1));
            encoder.encode_frame(frame).unwrap();
        }
    }

    #[test]
    fn animated_gif_keeps_every_frame_and_delay() {
        let path = scratch("anim.gif");
        write_gif(&path, &[200, 500, 40]);
        let raw = decode(&path).ok().unwrap();
        assert_eq!((raw.width, raw.height), (8, 6));
        assert_eq!(raw.animation.len(), 3);
        assert_eq!(raw.animation[0].delay, Duration::from_millis(200));
        assert_eq!(raw.animation[1].delay, Duration::from_millis(500));
        assert_ne!(raw.animation[0].rgba, raw.animation[1].rgba);
        assert_eq!(raw.rgba, raw.animation[0].rgba);
        assert_eq!(raw.format, "GIF");
    }

    #[test]
    fn tiny_gif_delays_fall_back_to_ten_fps() {
        let path = scratch("fast.gif");
        write_gif(&path, &[0, 10]);
        let raw = decode(&path).ok().unwrap();
        assert!(raw.animation.iter().all(|f| f.delay == DEFAULT_FRAME_DELAY));
    }

    #[test]
    fn single_frame_gif_is_a_still() {
        let path = scratch("still.gif");
        write_gif(&path, &[100]);
        let raw = decode(&path).ok().unwrap();
        assert!(raw.animation.is_empty());
        assert_eq!((raw.width, raw.height), (8, 6));
    }

    #[test]
    fn gif_thumbnail_is_a_single_frame() {
        let path = scratch("thumb.gif");
        write_gif(&path, &[100, 100]);
        let thumb = thumbnail(&path, 4).ok().unwrap();
        assert!(thumb.animation.is_empty());
    }
}
