//! Backgrounds behind the screen and the keys.
//!
//! Everything here works in panel coordinates. The info screen and the keys
//! are regions of one physical display, and the calibration says where each
//! one sits; a background is laid over the rectangle covering whichever of
//! them it spans, and each surface takes its own slice. That is what makes a
//! picture run continuously from the screen down through the keycaps instead
//! of being squeezed into each one separately.
//!
//! A frame is a single buffer over that rectangle, so a surface's slice is a
//! crop and a resize -- no surface ever draws the background itself. Drawn
//! animations are computed at a quarter of the panel's resolution, which is
//! indistinguishable for soft gradients and sixteen times cheaper; a still
//! picture is kept at full resolution; an animated GIF somewhere in between,
//! so its frames fit a memory budget.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use galdeck::{Canvas, Rgb};
use galdeck_model::{Backdrop, Motion, Span};
use image::{AnimationDecoder, RgbImage, RgbaImage};

/// Largest image file worth opening. A background is at most about a
/// megapixel on the panel; anything this size is a mistake or a camera raw.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Most frames of a GIF that are kept.
const MAX_GIF_FRAMES: usize = 240;
/// Memory a GIF's frames may take once scaled, in total.
const GIF_BUDGET_BYTES: f32 = 48.0 * 1024.0 * 1024.0;
/// Buffer pixels per panel pixel for a drawn animation.
const MOTION_SCALE: f32 = 0.25;
/// A GIF frame shorter than this is treated as this long: browsers do the
/// same, because many GIFs say 0 ms and mean "fast".
const MIN_GIF_DELAY: Duration = Duration::from_millis(20);

/// A rectangle on the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    fn right(self) -> i32 {
        self.x + self.width as i32
    }

    fn bottom(self) -> i32 {
        self.y + self.height as i32
    }

    fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            (self.right().max(other.right()) - x) as u32,
            (self.bottom().max(other.bottom()) - y) as u32,
        )
    }
}

/// Where the surfaces are on the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub lcd: Rect,
    /// One per key, by index: the part of the panel that key's image covers.
    pub keys: Vec<Rect>,
}

enum Source {
    /// Already scaled and dimmed.
    Still(Arc<RgbImage>),
    Frames {
        frames: Vec<Arc<RgbImage>>,
        /// When each frame ends, from the start of the loop.
        ends: Vec<Duration>,
    },
    Motion(Motion),
}

/// A background, ready to produce frames.
pub struct Scene {
    span: Span,
    /// The panel rectangle the buffer covers.
    extent: Rect,
    /// Buffer pixels per panel pixel.
    scale: f32,
    source: Source,
    geometry: Geometry,
    interval: Duration,
    speed: f32,
    dim: f32,
    colors: Vec<Rgb>,
}

impl Scene {
    /// Prepare a background. Loads and scales any image, so the cost is paid
    /// once rather than per frame.
    ///
    /// `colors` are the animation's colours, already resolved and defaulted.
    pub fn load(
        backdrop: &Backdrop,
        colors: Vec<Rgb>,
        geometry: Geometry,
    ) -> Result<Scene, String> {
        let span = backdrop.span;
        let extent = match span {
            Span::Lcd => geometry.lcd,
            Span::Keys => keys_extent(&geometry.keys),
            Span::Both => geometry.lcd.union(keys_extent(&geometry.keys)),
        };
        if extent.width == 0 || extent.height == 0 {
            return Err("the surfaces it spans have no size".into());
        }
        let dim = backdrop.dim();
        let interval = Duration::from_millis(1000 / u64::from(backdrop.fps()));
        let (source, scale) = match (&backdrop.image, backdrop.animation) {
            (Some(path), _) => load_image(path, extent, dim)?,
            (None, Some(motion)) => (Source::Motion(motion), motion_scale(motion)),
            (None, None) => return Err("it has neither an image nor an animation".into()),
        };
        Ok(Scene {
            span,
            extent,
            scale,
            source,
            geometry,
            interval,
            speed: backdrop.speed(),
            dim,
            colors,
        })
    }

    pub fn span(&self) -> Span {
        self.span
    }

    /// Whether it moves, and so needs a timer.
    pub fn is_animated(&self) -> bool {
        match &self.source {
            Source::Still(_) => false,
            Source::Frames { frames, .. } => frames.len() > 1,
            Source::Motion(_) => true,
        }
    }

    /// Time between frames.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The buffer for frame number `tick`.
    pub fn render(&self, tick: u64) -> Arc<RgbImage> {
        let elapsed = self.interval * u32::try_from(tick % u64::from(u32::MAX)).unwrap_or(0);
        match &self.source {
            Source::Still(image) => Arc::clone(image),
            Source::Frames { frames, ends } => {
                let total = *ends.last().expect("at least one frame");
                let at =
                    Duration::from_nanos((elapsed.as_nanos() % total.as_nanos().max(1)) as u64);
                let index = ends.iter().position(|end| at < *end).unwrap_or(0);
                Arc::clone(&frames[index])
            }
            Source::Motion(motion) => {
                let seconds = elapsed.as_secs_f32() * self.speed;
                Arc::new(draw_motion(
                    *motion,
                    self.extent,
                    self.scale,
                    seconds,
                    &self.colors,
                    self.dim,
                ))
            }
        }
    }

    /// The screen's slice of a frame, at the screen's size, if the
    /// background covers it.
    pub fn lcd(&self, frame: &RgbImage, size: (u32, u32)) -> Option<Canvas> {
        self.span
            .covers_lcd()
            .then(|| self.slice(frame, self.geometry.lcd, size))
    }

    /// A key's slice of a frame, at the size its image is drawn, if the
    /// background covers the keys.
    pub fn key(&self, frame: &RgbImage, index: u8, size: (u32, u32)) -> Option<Canvas> {
        if !self.span.covers_keys() {
            return None;
        }
        let rect = *self.geometry.keys.get(index as usize)?;
        Some(self.slice(frame, rect, size))
    }

    /// A surface's part of a frame, at the surface's size.
    ///
    /// Sampled straight out of the frame, bilinearly, rather than cropped
    /// and handed to a general-purpose resize: this runs for the screen and
    /// every key on every frame, and the general path was most of a frame's
    /// cost. Where no scaling is needed -- a still picture is kept at the
    /// panel's own resolution -- it is a row-by-row copy.
    fn slice(&self, frame: &RgbImage, rect: Rect, size: (u32, u32)) -> Canvas {
        let (width, height) = size;
        if width == 0 || height == 0 {
            return Canvas::filled(width, height, Rgb::BLACK);
        }
        let (frame_width, frame_height) = (frame.width() as usize, frame.height() as usize);
        let source = frame.as_raw();
        let mut out = vec![0u8; width as usize * height as usize * 3];

        // Where the rect's first pixel lands in the frame, and how far one
        // output pixel steps through it.
        let origin_x = (rect.x - self.extent.x) as f32 * self.scale;
        let origin_y = (rect.y - self.extent.y) as f32 * self.scale;
        let step_x = rect.width as f32 * self.scale / width as f32;
        let step_y = rect.height as f32 * self.scale / height as f32;

        let exact = (step_x - 1.0).abs() < 1e-4
            && (step_y - 1.0).abs() < 1e-4
            && origin_x.fract() == 0.0
            && origin_y.fract() == 0.0
            && origin_x >= 0.0
            && origin_y >= 0.0
            && origin_x as usize + width as usize <= frame_width
            && origin_y as usize + height as usize <= frame_height;
        if exact {
            let (ox, oy) = (origin_x as usize, origin_y as usize);
            for row in 0..height as usize {
                let from = ((oy + row) * frame_width + ox) * 3;
                let to = row * width as usize * 3;
                out[to..to + width as usize * 3]
                    .copy_from_slice(&source[from..from + width as usize * 3]);
            }
        } else {
            // One index and weight per column and per row, worked out once;
            // weights in 1/256ths so the inner loop is integer arithmetic.
            let axis =
                |count: u32, origin: f32, step: f32, limit: usize| -> Vec<(usize, usize, u32)> {
                    (0..count)
                        .map(|i| {
                            let at = (origin + (i as f32 + 0.5) * step - 0.5)
                                .clamp(0.0, (limit - 1) as f32);
                            let low = at.floor() as usize;
                            let high = (low + 1).min(limit - 1);
                            (low, high, ((at - low as f32) * 256.0) as u32)
                        })
                        .collect()
                };
            let columns = axis(width, origin_x, step_x, frame_width);
            let rows = axis(height, origin_y, step_y, frame_height);
            for (row, &(y0, y1, wy)) in rows.iter().enumerate() {
                let top = y0 * frame_width * 3;
                let bottom = y1 * frame_width * 3;
                let line = &mut out[row * width as usize * 3..(row + 1) * width as usize * 3];
                for (column, &(x0, x1, wx)) in columns.iter().enumerate() {
                    let (a, b) = (x0 * 3, x1 * 3);
                    for channel in 0..3 {
                        let blend = |left: u8, right: u8| {
                            u32::from(left) * (256 - wx) + u32::from(right) * wx
                        };
                        let upper = blend(source[top + a + channel], source[top + b + channel]);
                        let lower =
                            blend(source[bottom + a + channel], source[bottom + b + channel]);
                        line[column * 3 + channel] =
                            ((upper * (256 - wy) + lower * wy) >> 16) as u8;
                    }
                }
            }
        }
        Canvas::from_rgb(width, height, out)
            .unwrap_or_else(|_| Canvas::filled(width, height, Rgb::BLACK))
    }
}

fn keys_extent(keys: &[Rect]) -> Rect {
    keys.iter()
        .copied()
        .reduce(Rect::union)
        .unwrap_or(Rect::new(0, 0, 0, 0))
}

/// Load a still or animated image, scaled to cover `extent`.
fn load_image(path: &Path, extent: Rect, dim: f32) -> Result<(Source, f32), String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is {} MB; backgrounds are limited to {} MB",
            path.display(),
            metadata.len() / (1024 * 1024),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    let is_gif = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"));

    if is_gif {
        let file = std::io::BufReader::new(
            std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?,
        );
        let decoder = image::codecs::gif::GifDecoder::new(file)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut frames = Vec::new();
        let mut delays = Vec::new();
        for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
            let frame = frame.map_err(|e| format!("{}: {e}", path.display()))?;
            let (numerator, denominator) = frame.delay().numer_denom_ms();
            let delay = Duration::from_millis(u64::from(numerator / denominator.max(1)));
            delays.push(delay.max(MIN_GIF_DELAY));
            frames.push(frame.into_buffer());
        }
        if frames.is_empty() {
            return Err(format!("{} has no frames", path.display()));
        }
        // As sharp as the budget allows, and never sharper than the panel.
        let per_frame = extent.width as f32 * extent.height as f32 * 3.0;
        let scale = (GIF_BUDGET_BYTES / (per_frame * frames.len() as f32))
            .sqrt()
            .clamp(MOTION_SCALE, 1.0);
        let mut ends = Vec::with_capacity(delays.len());
        let mut end = Duration::ZERO;
        for delay in delays {
            end += delay;
            ends.push(end);
        }
        let frames = frames
            .iter()
            .map(|frame| Arc::new(cover(frame, extent, scale, dim)))
            .collect();
        return Ok((Source::Frames { frames, ends }, scale));
    }

    let image = image::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .to_rgba8();
    Ok((
        Source::Still(Arc::new(cover(&image, extent, 1.0, dim))),
        1.0,
    ))
}

/// Scale an image to cover `extent` at `scale`, crop the overflow evenly,
/// flatten any transparency onto black, and darken it.
fn cover(image: &RgbaImage, extent: Rect, scale: f32, dim: f32) -> RgbImage {
    let width = ((extent.width as f32 * scale).round() as u32).max(1);
    let height = ((extent.height as f32 * scale).round() as u32).max(1);
    let factor = (width as f32 / image.width().max(1) as f32)
        .max(height as f32 / image.height().max(1) as f32);
    let scaled_width = ((image.width() as f32 * factor).ceil() as u32).max(width);
    let scaled_height = ((image.height() as f32 * factor).ceil() as u32).max(height);
    let scaled = image::imageops::resize(
        image,
        scaled_width,
        scaled_height,
        image::imageops::FilterType::Triangle,
    );
    let (ox, oy) = ((scaled_width - width) / 2, (scaled_height - height) / 2);
    let keep = 1.0 - dim;
    RgbImage::from_fn(width, height, |x, y| {
        let [r, g, b, a] = scaled.get_pixel(x + ox, y + oy).0;
        let k = keep * f32::from(a) / 255.0;
        image::Rgb([
            (f32::from(r) * k) as u8,
            (f32::from(g) * k) as u8,
            (f32::from(b) * k) as u8,
        ])
    })
}

/// Buffer pixels per panel pixel for a drawn animation.
///
/// Soft gradients are indistinguishable at a quarter of the resolution.
/// Stars, streaks and bubble outlines are not: at a quarter a star is a
/// four-pixel block. Those get half.
fn motion_scale(motion: Motion) -> f32 {
    match motion {
        Motion::Starfield | Motion::Rain | Motion::Bubbles => 0.5,
        _ => MOTION_SCALE,
    }
}

/// A well-mixed 32-bit hash, for placing things pseudo-randomly but the same
/// way every frame.
fn hash(n: u32) -> u32 {
    let mut x = n.wrapping_mul(0x9E37_79B9);
    x ^= x >> 16;
    x = x.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 13;
    x = x.wrapping_mul(0xC2B2_AE35);
    x ^ (x >> 16)
}

/// `hash` as a number from 0 to 1.
fn unit(n: u32) -> f32 {
    hash(n) as f32 / u32::MAX as f32
}

/// Smooth value noise: random heights on a unit grid, eased between.
fn noise(x: f32, y: f32, seed: u32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (xf, yf) = (x - xi, y - yi);
    let corner = |dx: i32, dy: i32| {
        let (cx, cy) = (xi as i32 + dx, yi as i32 + dy);
        unit((cx as u32).wrapping_mul(73_856_093) ^ (cy as u32).wrapping_mul(19_349_663) ^ seed)
    };
    let ease = |t: f32| t * t * (3.0 - 2.0 * t);
    let (u, v) = (ease(xf), ease(yf));
    let top = corner(0, 0) + (corner(1, 0) - corner(0, 0)) * u;
    let bottom = corner(0, 1) + (corner(1, 1) - corner(0, 1)) * u;
    top + (bottom - top) * v
}

/// A colour from `colors` read as a ramp from the last (0) to the first (1),
/// the way heat reads from embers to flame.
fn ramp(colors: &[Rgb], at: f32) -> Rgb {
    let n = colors.len();
    if n < 2 {
        return colors.first().copied().unwrap_or(Rgb::BLACK);
    }
    let position = (1.0 - at.clamp(0.0, 1.0)) * (n - 1) as f32;
    let index = (position.floor() as usize).min(n - 2);
    colors[index].lerp(colors[index + 1], position - index as f32)
}

/// A colour from a smooth loop through `colors`, `at` in any range.
fn palette(colors: &[Rgb], at: f32) -> Rgb {
    let n = colors.len().max(1);
    let position = at.rem_euclid(1.0) * n as f32;
    let index = position.floor() as usize % n;
    let next = (index + 1) % n;
    // Cosine easing, so the loop has no visible seams at the stops.
    let t = (1.0 - (position.fract() * std::f32::consts::PI).cos()) / 2.0;
    colors[index].lerp(colors[next], t)
}

/// One aurora curtain for a frame: its colour, its thickness, and per
/// column where its middle is and how brightly it shimmers.
type Curtain = (Rgb, f32, Vec<(f32, f32)>);

/// One frame of a drawn background.
///
/// Positions are in panel pixels, whatever the buffer's resolution, so an
/// animation looks the same however much of the panel it spans. Anything
/// that depends only on the column, or only on the row, is worked out once
/// per frame rather than once per pixel: this runs ten times a second, and
/// the trigonometry was most of its cost.
fn draw_motion(
    motion: Motion,
    extent: Rect,
    scale: f32,
    seconds: f32,
    colors: &[Rgb],
    dim: f32,
) -> RgbImage {
    let width = ((extent.width as f32 * scale).round() as u32).max(1);
    let height = ((extent.height as f32 * scale).round() as u32).max(1);
    let fallback = [Rgb::new(136, 192, 208), Rgb::new(16, 18, 24)];
    let colors = if colors.len() >= 2 {
        colors
    } else {
        &fallback[..]
    };
    let keep = 1.0 - dim;
    let t = seconds;
    let xs: Vec<f32> = (0..width)
        .map(|x| extent.x as f32 + x as f32 / scale)
        .collect();
    let ys: Vec<f32> = (0..height)
        .map(|y| extent.y as f32 + y as f32 / scale)
        .collect();

    // The sparse ones -- stars, streaks, bubbles -- and fire are drawn into a
    // plain buffer over the darkest colour, then darkened like the rest.
    if matches!(
        motion,
        Motion::Starfield | Motion::Rain | Motion::Bubbles | Motion::Fire
    ) {
        let base = *colors.last().expect("two or more");
        let mut buffer = vec![base; width as usize * height as usize];
        let size = (extent.width as f32, extent.height as f32);
        let to_buffer = |px: f32, py: f32| ((px * scale) as i64, (py * scale) as i64);
        let mut blend = |x: i64, y: i64, color: Rgb, alpha: f32| {
            if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
                let at = y as usize * width as usize + x as usize;
                buffer[at] = buffer[at].lerp(color, alpha.clamp(0.0, 1.0));
            }
        };
        let lights = &colors[..colors.len() - 1];
        match motion {
            Motion::Starfield => {
                // Three depths: the near ones bigger, brighter and faster, so
                // the field has parallax rather than sliding as one sheet.
                let count = (size.0 * size.1 / 2400.0) as u32;
                for i in 0..count {
                    let depth = 1.0 + (unit(i * 5) * 3.0).floor();
                    let speed = 10.0 * depth * depth;
                    let x = (unit(i * 5 + 1) * size.0 - t * speed).rem_euclid(size.0);
                    let y = unit(i * 5 + 2) * size.1;
                    let twinkle =
                        0.55 + 0.45 * (t * (1.5 + unit(i * 5 + 3) * 3.0) + i as f32).sin();
                    let color = lights[i as usize % lights.len()].lerp(Rgb::WHITE, 0.5);
                    let (bx, by) = to_buffer(x, y);
                    blend(bx, by, color, twinkle);
                    if depth >= 3.0 {
                        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                            blend(bx + dx, by + dy, color, twinkle * 0.45);
                        }
                    }
                }
            }
            Motion::Rain => {
                // Streaks fall in columns; each column has its own speed and
                // length, and flickers in blocks the size of a character.
                const COLUMN: f32 = 14.0;
                const STREAK: f32 = 3.0;
                let columns = (size.0 / COLUMN).ceil() as u32;
                for column in 0..columns {
                    let speed = 110.0 + 220.0 * unit(column * 7);
                    let length = 140.0 + 300.0 * unit(column * 7 + 1);
                    let cycle = size.1 + length;
                    let head = (t * speed + unit(column * 7 + 2) * cycle).rem_euclid(cycle);
                    let color = lights[column as usize % lights.len()];
                    let left = column as f32 * COLUMN;
                    let (x0, _) = to_buffer(left, 0.0);
                    let (x1, _) = to_buffer(left + STREAK, 0.0);
                    for by in 0..height as i64 {
                        let py = by as f32 / scale;
                        let behind = head - py;
                        if !(0.0..length).contains(&behind) {
                            continue;
                        }
                        let fade = (1.0 - behind / length).powf(1.5);
                        let glyph = (py / 12.0).floor() as u32;
                        let flicker = 0.6
                            + 0.4
                                * unit(
                                    column.wrapping_mul(977)
                                        ^ glyph.wrapping_mul(131)
                                        ^ (t * 8.0) as u32,
                                );
                        // The leading character is nearly white, as it is in
                        // the film.
                        let tint = if behind < 12.0 {
                            color.lerp(Rgb::WHITE, 0.7)
                        } else {
                            color
                        };
                        for bx in x0..x1.max(x0 + 1) {
                            blend(bx, by, tint, fade * flicker);
                        }
                    }
                }
            }
            Motion::Bubbles => {
                let count = (size.0 * size.1 / 9000.0) as u32;
                for i in 0..count {
                    let radius = 8.0 + 28.0 * unit(i * 5);
                    let speed = 20.0 + 45.0 * unit(i * 5 + 1);
                    let travel = size.1 + radius * 2.0;
                    let x = unit(i * 5 + 2) * size.0 + (t * 0.8 + i as f32).sin() * 12.0;
                    let y =
                        size.1 + radius - (t * speed + unit(i * 5 + 3) * travel).rem_euclid(travel);
                    let color = lights[i as usize % lights.len()];
                    let r = radius * scale;
                    let (cx, cy) = (x * scale, y * scale);
                    for by in (cy - r - 1.0) as i64..=(cy + r + 1.0) as i64 {
                        for bx in (cx - r - 1.0) as i64..=(cx + r + 1.0) as i64 {
                            let d = (bx as f32 - cx).hypot(by as f32 - cy);
                            if d > r + 1.0 {
                                continue;
                            }
                            // A bright rim, a faint body, and a glint.
                            let rim = (1.0 - (d - r).abs()).clamp(0.0, 1.0) * 0.8;
                            let body = if d < r { 0.12 } else { 0.0 };
                            blend(bx, by, color, rim.max(body));
                            let glint =
                                (bx as f32 - (cx - r * 0.35)).hypot(by as f32 - (cy - r * 0.35));
                            if glint < r * 0.2 {
                                blend(bx, by, Rgb::WHITE, 0.45);
                            }
                        }
                    }
                }
            }
            Motion::Fire => {
                // Heat falls off with height; two octaves of noise scrolling
                // upwards make the flames lick.
                for by in 0..height as usize {
                    let py = extent.y as f32 + by as f32 / scale;
                    let height_up = (extent.y as f32 + size.1 - py) / size.1;
                    for bx in 0..width as usize {
                        let px = extent.x as f32 + bx as f32 / scale;
                        let n = noise(px / 70.0, py / 55.0 + t * 1.6, 11) * 0.65
                            + noise(px / 28.0, py / 22.0 + t * 3.1, 23) * 0.35;
                        let heat = (1.25 - height_up * 1.9 + (n - 0.5) * 1.1).clamp(0.0, 1.0);
                        buffer[by * width as usize + bx] = ramp(colors, heat);
                    }
                }
            }
            _ => unreachable!("only the sparse motions are drawn here"),
        }
        let mut image = RgbImage::new(width, height);
        for (pixel, color) in image.pixels_mut().zip(buffer) {
            let [r, g, b] = color.to_array();
            *pixel = image::Rgb([
                (f32::from(r) * keep) as u8,
                (f32::from(g) * keep) as u8,
                (f32::from(b) * keep) as u8,
            ]);
        }
        return image;
    }

    let mut image = RgbImage::new(width, height);
    let mut put = |x: u32, y: u32, color: Rgb| {
        let [r, g, b] = color.to_array();
        image.put_pixel(
            x,
            y,
            image::Rgb([
                (f32::from(r) * keep) as u8,
                (f32::from(g) * keep) as u8,
                (f32::from(b) * keep) as u8,
            ]),
        );
    };

    match motion {
        Motion::Gradient => {
            let (sin, cos) = (t * 0.15).sin_cos();
            for (y, py) in ys.iter().enumerate() {
                for (x, px) in xs.iter().enumerate() {
                    let along = px * cos + py * sin;
                    put(
                        x as u32,
                        y as u32,
                        palette(colors, along / 900.0 + t * 0.03),
                    );
                }
            }
        }
        Motion::Waves => {
            let sway: Vec<f32> = xs
                .iter()
                .map(|px| (px / 160.0 + t * 0.9).sin() * 1.4)
                .collect();
            for (y, py) in ys.iter().enumerate() {
                for (x, sway) in sway.iter().enumerate() {
                    let v = (py / 70.0 + sway + t * 0.6).sin();
                    put(x as u32, y as u32, palette(colors, v * 0.25 + py / 1600.0));
                }
            }
        }
        Motion::Plasma => {
            let across: Vec<f32> = xs.iter().map(|px| (px / 90.0 + t).sin()).collect();
            for (y, py) in ys.iter().enumerate() {
                let down = (py / 110.0 + t * 1.3).sin();
                for (x, px) in xs.iter().enumerate() {
                    let v = across[x]
                        + down
                        + ((px + py) / 140.0 + t * 0.7).sin()
                        + ((px - 360.0).hypot(py - 640.0) / 80.0 - t).sin();
                    put(x as u32, y as u32, palette(colors, v / 8.0 + t * 0.02));
                }
            }
        }
        Motion::Aurora => {
            // Up to three curtains of light over the darkest colour. Each
            // curtain's height and shimmer depend only on the column.
            let base = *colors.last().expect("two or more");
            let curtains = (colors.len() - 1).min(3);
            let shape: Vec<Curtain> = colors
                .iter()
                .take(curtains)
                .enumerate()
                .map(|(i, curtain)| {
                    let i = i as f32;
                    let thickness = 90.0 + 30.0 * (t * 0.3 + i).sin();
                    let columns = xs
                        .iter()
                        .map(|px| {
                            let centre = 180.0
                                + i * 260.0
                                + (px / 180.0 + t * 0.5 + i * 1.7).sin() * 110.0
                                + (px / 67.0 - t * 0.8 + i).sin() * 30.0;
                            let shimmer = 0.65 + 0.35 * (px / 45.0 + t * 1.1 + i * 2.3).sin();
                            (centre, shimmer)
                        })
                        .collect();
                    (*curtain, thickness, columns)
                })
                .collect();
            for (y, py) in ys.iter().enumerate() {
                for x in 0..xs.len() {
                    let mut color = base;
                    for (curtain, thickness, columns) in &shape {
                        let (centre, shimmer) = columns[x];
                        let distance = (py - centre) / thickness;
                        // Past three widths the glow is under 1/8000: not
                        // worth an exp.
                        if distance.abs() < 3.0 {
                            let glow = (-distance * distance).exp() * shimmer;
                            color = color.lerp(*curtain, glow.clamp(0.0, 1.0) * 0.85);
                        }
                    }
                    put(x as u32, y as u32, color);
                }
            }
        }
        _ => unreachable!("the sparse motions returned above"),
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        // A screen across the top and two keys below it, like a miniature
        // panel.
        Geometry {
            lcd: Rect::new(0, 0, 200, 100),
            keys: vec![Rect::new(0, 120, 80, 80), Rect::new(120, 120, 80, 80)],
        }
    }

    fn still(path: &Path, span: Span) -> Scene {
        let backdrop = Backdrop {
            span,
            image: Some(path.to_path_buf()),
            dim: Some(0.0),
            ..Backdrop::default()
        };
        Scene::load(&backdrop, Vec::new(), geometry()).unwrap()
    }

    /// A picture that is red at the top of the panel and blue at the bottom.
    fn two_tone(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "galdeck-backdrop-{name}-{}.png",
            std::process::id()
        ));
        RgbaImage::from_fn(200, 200, |_, y| {
            if y < 100 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        })
        .save(&path)
        .unwrap();
        path
    }

    #[test]
    fn one_picture_runs_across_the_screen_and_the_keys() {
        let scene = still(&two_tone("both"), Span::Both);
        let frame = scene.render(0);
        let lcd = scene.lcd(&frame, (200, 100)).unwrap();
        let key = scene.key(&frame, 0, (80, 80)).unwrap();
        // The screen is the top of the picture, the keys the bottom -- not
        // the whole picture squeezed into each.
        assert_eq!(lcd.pixel(100, 50), Some(Rgb::new(255, 0, 0)));
        assert_eq!(key.pixel(40, 40), Some(Rgb::new(0, 0, 255)));
        assert!(!scene.is_animated());
    }

    #[test]
    fn a_background_only_gives_the_surfaces_it_spans() {
        let scene = still(&two_tone("lcd"), Span::Lcd);
        let frame = scene.render(0);
        assert!(scene.lcd(&frame, (200, 100)).is_some());
        assert!(scene.key(&frame, 0, (80, 80)).is_none());

        let scene = still(&two_tone("keys"), Span::Keys);
        let frame = scene.render(0);
        assert!(scene.lcd(&frame, (200, 100)).is_none());
        // Spanning only the keys, the picture is stretched over them alone,
        // so the top of it is on the keys now.
        assert_eq!(
            scene.key(&frame, 0, (80, 80)).unwrap().pixel(40, 5),
            Some(Rgb::new(255, 0, 0))
        );
    }

    #[test]
    fn dimming_darkens() {
        let path = two_tone("dim");
        let backdrop = Backdrop {
            span: Span::Lcd,
            image: Some(path),
            dim: Some(0.5),
            ..Backdrop::default()
        };
        let scene = Scene::load(&backdrop, Vec::new(), geometry()).unwrap();
        let frame = scene.render(0);
        // The screen is half as tall as the picture, so it shows the middle
        // of it: near its top edge is still the red half.
        let pixel = scene
            .lcd(&frame, (200, 100))
            .unwrap()
            .pixel(100, 10)
            .unwrap();
        assert_eq!(pixel, Rgb::new(127, 0, 0));
    }

    #[test]
    fn a_drawn_animation_moves_and_fills_every_surface() {
        for motion in [
            Motion::Aurora,
            Motion::Gradient,
            Motion::Waves,
            Motion::Plasma,
            Motion::Starfield,
            Motion::Rain,
            Motion::Fire,
            Motion::Bubbles,
        ] {
            let backdrop = Backdrop {
                animation: Some(motion),
                ..Backdrop::default()
            };
            let colors = vec![
                Rgb::new(200, 0, 200),
                Rgb::new(0, 200, 200),
                Rgb::new(10, 10, 20),
            ];
            let scene = Scene::load(&backdrop, colors, geometry()).unwrap();
            assert!(scene.is_animated());
            let a = scene.render(0);
            let b = scene.render(15);
            assert_ne!(a.as_raw(), b.as_raw(), "{motion:?} did not move");
            assert!(scene.lcd(&a, (200, 100)).is_some());
            assert!(scene.key(&a, 1, (80, 80)).is_some());
        }
    }

    #[test]
    fn an_animated_gif_plays_by_its_own_timing() {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame};
        let path =
            std::env::temp_dir().join(format!("galdeck-backdrop-{}.gif", std::process::id()));
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = GifEncoder::new(file);
            for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
                let frame = Frame::from_parts(
                    RgbaImage::from_pixel(20, 20, image::Rgba(color)),
                    0,
                    0,
                    Delay::from_numer_denom_ms(200, 1),
                );
                encoder.encode_frame(frame).unwrap();
            }
        }
        let backdrop = Backdrop {
            span: Span::Lcd,
            image: Some(path),
            fps: Some(10),
            dim: Some(0.0),
            ..Backdrop::default()
        };
        let scene = Scene::load(&backdrop, Vec::new(), geometry()).unwrap();
        assert!(scene.is_animated());
        let red = |tick| {
            scene
                .lcd(&scene.render(tick), (20, 10))
                .unwrap()
                .pixel(5, 5)
                .unwrap()
                .r
                > 200
        };
        // 100 ms a tick, 200 ms a frame: two ticks of each.
        assert!(red(0));
        assert!(red(1));
        assert!(!red(2));
        assert!(red(4));
    }

    #[test]
    fn a_missing_image_is_an_error_not_a_panic() {
        let backdrop = Backdrop {
            image: Some("/definitely/not/here.png".into()),
            ..Backdrop::default()
        };
        assert!(Scene::load(&backdrop, Vec::new(), geometry()).is_err());
    }
}
