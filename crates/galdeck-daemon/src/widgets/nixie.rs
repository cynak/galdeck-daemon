//! Nixie tubes: a glowing tube for each digit of the time or of a reading,
//! standing in a dark room that is lit mostly by the tubes themselves.
//!
//! Everything that moves -- the glow's shimmer, the odd tube flickering, the
//! haze and the dust drifting through the light -- is a pure function of the
//! time passed in, so a frame can be drawn again exactly and a test can pick
//! the moment it looks at.
//!
//! Most of a frame does not move at all: the room, the glass, the mesh and
//! the cathodes nobody lit. That is drawn once per tile and kept, as is each
//! digit's glow for a size, and a frame is that picture with the light added
//! on top -- which is what keeps ten frames a second cheap.
//!
//! The digits are wire outlines, as real cathodes are, with every other
//! digit's wire behind the lit one: a nixie tube stacks all ten, and they
//! show as a dark tangle against its glow, behind the fine mesh of the anode.
//!
//! The tubes come from the formatted time rather than from the clock itself,
//! so `format` decides how many there are: `%H:%M:%S` is six, `%H:%M` four,
//! and `%l:%M` leaves the first tube dark before ten o'clock.
//!
//! A reading's unit goes on a symbol tube, as it did on real IN-19s: `42%`
//! is three tubes, `63°C` three, and the `12M` of a network rate three. A
//! symbol tube stacks its symbols as a digit tube stacks its digits. The
//! title is lit as well: its letters glow as neon, as the wires do.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::Hash;
use std::rc::Rc;

use galdeck::{Align, Canvas, Font, Rgb};
use galdeck_model::Widget;

use super::draw::{self, Area, Colors};

/// Neon: what a lit cathode glows.
const NEON: Rgb = Rgb::new(255, 122, 34);
/// The heart of a lit wire, where the glow is brightest and yellowest.
const HOT: Rgb = Rgb::new(255, 170, 95);
/// The room the tubes stand in: in the middle, and in the corners.
const ROOM: Rgb = Rgb::new(19, 15, 12);
const ROOM_EDGE: Rgb = Rgb::new(6, 5, 4);
/// Glass: the body darkens what is behind it a little, and the rim and the
/// sheen catch what light there is.
const GLASS_BODY: Rgb = Rgb::new(9, 7, 6);
const GLASS_RIM: Rgb = Rgb::new(118, 88, 66);
const SHEEN: Rgb = Rgb::new(160, 136, 116);
/// The cathodes nobody lit, and the wires of the anode mesh in front.
const CAGE_METAL: Rgb = Rgb::new(72, 58, 48);
const MESH_WIRE: Rgb = Rgb::new(4, 3, 3);
/// Smoke and dust, where the tubes light them.
const HAZE: Rgb = Rgb::new(255, 176, 120);
const DUST: Rgb = Rgb::new(255, 204, 156);

/// Room between two tubes, and the room a separator takes, in tube widths.
const TUBE_GAP: f32 = 0.12;
const SEPARATOR_GAP: f32 = 0.38;
/// A tube's height, in tube widths.
const TUBE_ASPECT: f32 = 1.75;
/// How much of the glow a wire of the mesh, and an unlit cathode, stop.
const MESH_SHADOW: f32 = 0.5;
const CAGE_SHADOW: f32 = 0.7;
/// How strongly the tubes light the room behind them, and the haze in it.
const SPILL: f32 = 0.055;
const HAZE_STRENGTH: f32 = 0.06;
/// How often the haze moves on. It drifts a few pixels a second across a
/// field with no edges, so a step this long is too small to see, and every
/// frame in between reuses the same picture of it.
const HAZE_STEP: f64 = 0.5;
/// Where light stops simply adding and starts to roll off towards white.
const KNEE: f32 = 180.0;
/// How long each chance of a flicker lasts, in seconds.
const FLICKER_WINDOW: f64 = 6.0;

/// The time to animate by, in seconds.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// What a character of the formatted time becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Glyph {
    /// A tube, lit with a cathode -- a digit, or past nine a symbol -- or
    /// left dark.
    Tube(Option<u8>),
    /// The space between groups of tubes, with this many lamps in it: two
    /// for a `:`, one low down for a `.`, none for anything else.
    Separator(u8),
}

/// The symbol cathodes, numbered on from the ten digits. Real IN-19 tubes
/// carried symbols like these for readings rather than the time: a percent
/// sign, `k` and `M`, a sign, and on one model `°C`.
const PERCENT: u8 = 10;
const DEGREE: u8 = 11;
const CELSIUS: u8 = 12;
const FAHRENHEIT: u8 = 13;
const KILO: u8 = 14;
const MEGA: u8 = 15;
const GIGA: u8 = 16;
const PLUS: u8 = 17;
const MINUS: u8 = 18;
/// Every symbol, stacked in a symbol tube as the digits are in a digit tube.
const SYMBOLS: std::ops::RangeInclusive<u8> = PERCENT..=MINUS;

fn is_symbol(cathode: u8) -> bool {
    cathode >= PERCENT
}

/// The tubes and separators for `text`.
fn glyphs(text: &str) -> Vec<Glyph> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Glyph> = Vec::new();
    // Set when a symbol has taken the character after it too: the `C` of `°C`.
    let mut taken = false;
    for (i, &c) in chars.iter().enumerate() {
        if std::mem::take(&mut taken) {
            continue;
        }
        let before = i.checked_sub(1).map(|j| chars[j]);
        let after = chars.get(i + 1);
        // A unit's letter is a tube only straight after a number: the `M` of
        // `12M` is, the `M` of `PM` is not.
        let after_number = before.is_some_and(|b| b.is_ascii_digit());
        let glyph = match c {
            '0'..='9' => Glyph::Tube(c.to_digit(10).map(|d| d as u8)),
            // The padding `%k` and `%l` put before a single digit: the tube
            // is there, just not lit. A space after a word is only a space.
            ' ' if after.is_some_and(char::is_ascii_digit)
                && !before.is_some_and(char::is_alphanumeric) =>
            {
                Glyph::Tube(None)
            }
            ':' => Glyph::Separator(2),
            '.' => Glyph::Separator(1),
            '%' => Glyph::Tube(Some(PERCENT)),
            '°' => Glyph::Tube(Some(match after {
                Some('C' | 'c') => {
                    taken = true;
                    CELSIUS
                }
                Some('F' | 'f') => {
                    taken = true;
                    FAHRENHEIT
                }
                _ => DEGREE,
            })),
            'K' | 'k' if after_number => Glyph::Tube(Some(KILO)),
            'M' if after_number => Glyph::Tube(Some(MEGA)),
            'G' if after_number => Glyph::Tube(Some(GIGA)),
            // A sign before a number, not the dash between two a date has.
            '+' | '-' | '−' if !after_number && after.is_some_and(char::is_ascii_digit) => {
                Glyph::Tube(Some(if c == '+' { PLUS } else { MINUS }))
            }
            // A weekday, AM or PM: there is no tube that shows letters.
            c if c.is_alphabetic() => continue,
            _ => Glyph::Separator(0),
        };
        match (out.last_mut(), glyph) {
            // Separators in a row are one, with the most lamps of any.
            (Some(Glyph::Separator(last)), Glyph::Separator(lamps)) => *last = (*last).max(lamps),
            (None, Glyph::Separator(_)) => {}
            _ => out.push(glyph),
        }
    }
    if matches!(out.last(), Some(Glyph::Separator(_))) {
        out.pop();
    }
    out
}

/// Draw `text` -- the time or a reading, already formatted -- at animation
/// time `t`.
///
/// The neon is orange unless the widget has a `color`, and the tubes stand
/// in a dark room unless it has a background or an image of its own.
pub fn draw(
    canvas: &mut Canvas,
    area: Area,
    widget: &Widget,
    text: &str,
    colors: Colors,
    font: Option<&Font>,
    t: f64,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let look = Look::of(widget, colors);
    let caption = widget.title.as_deref().filter(|t| !t.is_empty()).zip(font);
    let layout = Layout::new(area.width, area.height, glyphs(text), caption.is_some());
    // Drawn straight into the canvas's pixels, lent out and handed back: a
    // frame touches a few hundred thousand of them, and going through the
    // canvas's checked accessors for each costs more than the drawing does.
    let (width, height) = (canvas.width(), canvas.height());
    let mut pixels = std::mem::replace(canvas, Canvas::new(0, 0)).into_rgb();
    let mut s = Surface {
        pixels: &mut pixels,
        width,
        height,
        area,
    };
    still(&mut s, &layout, look, t);
    light_up(&mut s, &layout, look, t);
    if let (Some((title, font)), Some(at)) = (caption, layout.caption) {
        caption_light(&mut s, font, title, at, layout.inner_width, look, t);
    }
    *canvas = Canvas::from_rgb(width, height, pixels).expect("a canvas's own pixels fit it");
}

/// The colours a clock is drawn in.
#[derive(Clone, Copy)]
struct Look {
    neon: Rgb,
    hot: Rgb,
    /// How opaque the room behind the tubes is: none at all when the widget
    /// brings a background of its own.
    room: f32,
}

impl Look {
    fn of(widget: &Widget, colors: Colors) -> Self {
        // Its own colour, or its theme's for clocks: either way, asked for.
        let own = widget.color().is_some();
        let neon = if own { colors.accent } else { NEON };
        Self {
            neon,
            hot: if own {
                neon.lerp(Rgb::new(255, 255, 255), 0.45)
            } else {
                HOT
            },
            room: if widget.background().is_none() && widget.image.is_none() {
                widget.opacity()
            } else {
                0.0
            },
        }
    }
}

/// Where everything goes in an area, from its top left.
struct Layout {
    /// Each glyph, and where its left edge is.
    placed: Vec<(Glyph, f32)>,
    tube_w: f32,
    tube_h: f32,
    top: f32,
    inner_width: u32,
    /// Where a caption's middle is, and how big it is, if there is one.
    caption: Option<(f32, f32)>,
}

impl Layout {
    fn new(width: u32, height: u32, glyphs: Vec<Glyph>, captioned: bool) -> Self {
        let inset = (width.min(height) / 16).max(1);
        let inner_w = width.saturating_sub(inset * 2) as f32;
        let inner_h = height.saturating_sub(inset * 2) as f32;
        let caption_size = (inner_h * 0.13).clamp(9.0, 26.0);
        let caption_room = if captioned { caption_size * 1.6 } else { 0.0 };

        // Where each glyph starts, in tube widths from the left of the first.
        let mut starts = Vec::with_capacity(glyphs.len());
        let mut units = 0.0;
        for (i, glyph) in glyphs.iter().enumerate() {
            starts.push(units);
            units += match glyph {
                Glyph::Tube(_) if matches!(glyphs.get(i + 1), Some(Glyph::Tube(_))) => {
                    1.0 + TUBE_GAP
                }
                Glyph::Tube(_) => 1.0,
                Glyph::Separator(_) => SEPARATOR_GAP,
            };
        }
        let tube_w = (inner_w / units.max(1.0))
            .min((inner_h - caption_room) / TUBE_ASPECT)
            .max(0.0);
        let tube_h = if glyphs.is_empty() {
            0.0
        } else {
            tube_w * TUBE_ASPECT
        };
        let top = inset as f32 + (inner_h - tube_h - caption_room) / 2.0;
        let left = inset as f32 + (inner_w - tube_w * units) / 2.0;
        Self {
            placed: glyphs
                .into_iter()
                .zip(starts)
                .map(|(glyph, start)| (glyph, left + start * tube_w))
                .collect(),
            tube_w,
            tube_h,
            top,
            inner_width: width.saturating_sub(inset * 2),
            caption: captioned.then_some((top + tube_h + caption_room / 2.0, caption_size)),
        }
    }

    /// Whether the tubes are big enough to draw at all.
    fn drawable(&self) -> bool {
        self.tube_w >= 4.0
    }

    /// Each tube, and the digit it shows.
    fn tubes(&self) -> impl Iterator<Item = (Tube, Option<u8>)> + '_ {
        self.placed
            .iter()
            .filter_map(move |&(glyph, x)| match glyph {
                Glyph::Tube(digit) => Some((
                    Tube {
                        x: x.round() as i32,
                        y: self.top.round() as i32,
                        w: self.tube_w.round() as u32,
                        h: self.tube_h.round() as u32,
                        symbols: digit.is_some_and(is_symbol),
                    },
                    digit,
                )),
                Glyph::Separator(_) => None,
            })
    }

    /// Each separator with lamps: which glyph it is, the middle of it, and
    /// how many lamps it has.
    fn separators(&self) -> impl Iterator<Item = (usize, f32, u8)> + '_ {
        self.placed
            .iter()
            .enumerate()
            .filter_map(move |(i, &(glyph, x))| match glyph {
                Glyph::Separator(count) if count > 0 => {
                    Some((i, x + self.tube_w * SEPARATOR_GAP / 2.0, count))
                }
                _ => None,
            })
    }

    /// The light the lit tubes throw on the room.
    fn lights(&self) -> Lights {
        let centres = if self.drawable() {
            self.tubes()
                .filter(|(_, digit)| digit.is_some())
                .map(|(tube, _)| {
                    (
                        tube.x as f32 + tube.w as f32 / 2.0,
                        tube.y as f32 + tube.h as f32 * 0.52,
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        Lights {
            centres,
            reach: (self.tube_w * 0.9, self.tube_h * 0.55),
        }
    }

    /// The pixel a separator's lamp is centred on, for the separator at
    /// `x` and a lamp `share` of the way down the tubes.
    fn lamp(&self, x: f32, share: f32) -> (i32, i32) {
        (
            x.floor() as i32,
            (self.top + self.tube_h * share).floor() as i32,
        )
    }

    /// How big a puff of smoke is.
    fn haze_scale(&self) -> f32 {
        (self.tube_h * 0.8).max(24.0)
    }

    /// What the still part of the picture depends on, beyond its size.
    fn signature(&self) -> Vec<u8> {
        self.placed
            .iter()
            .map(|(glyph, _)| match *glyph {
                Glyph::Tube(Some(c)) if is_symbol(c) => b'S',
                Glyph::Tube(Some(_)) => b'T',
                Glyph::Tube(None) => b'D',
                Glyph::Separator(count) => b'0' + count,
            })
            .collect()
    }
}

/// Where a separator's lamps are, as shares of a tube's height from its top.
fn lamp_heights(count: u8) -> &'static [f32] {
    match count {
        0 => &[],
        1 => &[0.8],
        _ => &[0.41, 0.61],
    }
}

/// The light the lit tubes throw on the room: a soft pool around each.
struct Lights {
    centres: Vec<(f32, f32)>,
    reach: (f32, f32),
}

impl Lights {
    /// How lit a point is: about 1 beside a tube, more between two.
    fn at(&self, x: f32, y: f32) -> f32 {
        self.centres
            .iter()
            .map(|&(cx, cy)| {
                let (dx, dy) = ((x - cx) / self.reach.0, (y - cy) / self.reach.1);
                (-0.5 * (dx * dx + dy * dy)).exp()
            })
            .sum::<f32>()
            .min(1.5)
    }

    /// The same for every pixel of a `width` by `height` area, row by row.
    fn field(&self, width: u32, height: u32) -> Vec<f32> {
        // Each pool is a Gaussian, which splits into a row of factors and a
        // column of them: a multiply per pixel per tube, not an exp.
        let factors = |centre: f32, reach: f32, n: u32| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    let d = (i as f32 + 0.5 - centre) / reach;
                    (-0.5 * d * d).exp()
                })
                .collect()
        };
        let columns: Vec<Vec<f32>> = self
            .centres
            .iter()
            .map(|&(cx, _)| factors(cx, self.reach.0, width))
            .collect();
        let rows: Vec<Vec<f32>> = self
            .centres
            .iter()
            .map(|&(_, cy)| factors(cy, self.reach.1, height))
            .collect();
        let mut out = Vec::with_capacity((width * height) as usize);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let sum: f32 = columns.iter().zip(&rows).map(|(c, r)| c[x] * r[y]).sum();
                out.push(sum.min(1.5));
            }
        }
        out
    }
}

/// A canvas's pixels, drawn on in one area of it from the area's top left,
/// with nothing reaching past its edges: a glow near the edge of a tile must
/// not spill onto the next one.
struct Surface<'a> {
    /// Every pixel of the canvas, three bytes each, row by row.
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
    area: Area,
}

impl Surface<'_> {
    /// Where a point of the area is among the pixels, if it is in the area
    /// and on the canvas.
    fn at(&self, x: i32, y: i32) -> Option<usize> {
        let in_area = x >= 0 && y >= 0 && x < self.area.width as i32 && y < self.area.height as i32;
        let (cx, cy) = (self.area.x + x, self.area.y + y);
        let on_canvas = cx >= 0 && cy >= 0 && cx < self.width as i32 && cy < self.height as i32;
        (in_area && on_canvas).then(|| (cy as usize * self.width as usize + cx as usize) * 3)
    }

    /// Blend a pixel towards `color`, exactly as the canvas would.
    fn blend(&mut self, x: i32, y: i32, color: Rgb, alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let Some(i) = self.at(x, y) else {
            return;
        };
        let alpha = alpha.min(1.0);
        for (channel, target) in self.pixels[i..i + 3]
            .iter_mut()
            .zip([color.r, color.g, color.b])
        {
            let base = f32::from(*channel);
            *channel = (base + (f32::from(target) - base) * alpha) as u8;
        }
    }

    /// Light added to a pixel, as glow adds to what is behind it.
    fn add(&mut self, x: i32, y: i32, light: [f32; 3]) {
        // Less than a level adds nothing once rounded down; most of a
        // halo's outskirts stop here.
        if light.iter().all(|&level| level < 1.0) {
            return;
        }
        if let Some(i) = self.at(x, y) {
            lift(&mut self.pixels[i..i + 3], light);
        }
    }

    /// The light of a sprite `span` pixels wide and `rows` high with its top
    /// left at `(x, y)`: `light(i)` for its `i`th pixel. It is clipped a row
    /// at a time rather than a pixel at a time, since this is most of what
    /// a frame costs.
    fn add_sprite(
        &mut self,
        (x, y): (i32, i32),
        (span, rows): (u32, u32),
        light: impl Fn(usize) -> [f32; 3],
    ) {
        // The part of the area that is on the canvas, from its top left...
        let columns = (
            (-self.area.x).max(0),
            (self.width as i32 - self.area.x).min(self.area.width as i32),
        );
        let lines = (
            (-self.area.y).max(0),
            (self.height as i32 - self.area.y).min(self.area.height as i32),
        );
        // ...and so the sprite's pixels that land on it.
        let (first, last) = ((columns.0 - x).max(0), (columns.1 - x).min(span as i32));
        for sy in (lines.0 - y).max(0)..(lines.1 - y).min(rows as i32) {
            let row = (self.area.y + y + sy) as usize * self.width as usize;
            let start = sy as usize * span as usize;
            for sx in first..last {
                let light = light(start + sx as usize);
                if light.iter().all(|&level| level < 1.0) {
                    continue;
                }
                let i = (row + (self.area.x + x + sx) as usize) * 3;
                lift(&mut self.pixels[i..i + 3], light);
            }
        }
    }

    /// Lay a picture the size of the area over it, a row at a time.
    fn paste(&mut self, picture: &[u8]) {
        let (w, h) = (self.area.width as i32, self.area.height as i32);
        // The columns of the area that are on the canvas.
        let from = (-self.area.x).max(0);
        let to = (self.width as i32 - self.area.x).min(w);
        if from >= to {
            return;
        }
        for y in 0..h {
            let row = self.area.y + y;
            if row < 0 || row >= self.height as i32 {
                continue;
            }
            let source = ((y * w + from) * 3) as usize..((y * w + to) * 3) as usize;
            let start = (row as usize * self.width as usize + (self.area.x + from) as usize) * 3;
            self.pixels[start..start + source.len()].copy_from_slice(&picture[source]);
        }
    }
}

/// Lay down everything that does not move, and the haze, which barely does:
/// the picture kept of them when the room is opaque, and otherwise drawn
/// afresh over whatever is behind.
fn still(s: &mut Surface, layout: &Layout, look: Look, t: f64) {
    let epoch = (t / HAZE_STEP).floor() * HAZE_STEP;
    if look.room < 1.0 {
        paint_room(s, layout, look, epoch);
        return;
    }
    let (w, h) = (s.area.width, s.area.height);
    let key = SceneKey {
        size: (w, h),
        glyphs: layout.signature(),
        neon: (look.neon.r, look.neon.g, look.neon.b),
        caption: layout.caption.is_some(),
    };
    let area = Area::new(0, 0, w, h);
    let scene = scene(key.clone(), || {
        let mut pixels = Canvas::filled(w, h, ROOM).into_rgb();
        let mut own = Surface {
            pixels: &mut pixels,
            width: w,
            height: h,
            area,
        };
        paint_still(&mut own, layout, look);
        pixels
    });
    let hazy = hazy(key, epoch, || {
        let mut pixels = scene.to_vec();
        let mut own = Surface {
            pixels: &mut pixels,
            width: w,
            height: h,
            area,
        };
        haze(&mut own, &layout.lights(), layout.haze_scale(), epoch, look);
        pixels
    });
    s.paste(&hazy);
}

/// The room with everything still in it, and the haze as it is at `epoch`.
fn paint_room(s: &mut Surface, layout: &Layout, look: Look, epoch: f64) {
    paint_still(s, layout, look);
    haze(s, &layout.lights(), layout.haze_scale(), epoch, look);
}

/// The room, and the tubes and lamps in it, unlit.
fn paint_still(s: &mut Surface, layout: &Layout, look: Look) {
    let (w, h) = (s.area.width, s.area.height);
    // Dark and warm, darkest in the corners, and lit around the tubes.
    let field = layout.lights().field(w, h);
    for y in 0..h {
        for x in 0..w {
            let (px, py) = (x as i32, y as i32);
            if look.room > 0.0 {
                let nx = (x as f32 + 0.5) / w as f32 * 2.0 - 1.0;
                let ny = (y as f32 + 0.5) / h as f32 * 2.0 - 1.0;
                let open = (1.0 - 0.6 * (0.55 * nx * nx + ny * ny)).clamp(0.0, 1.0);
                s.blend(px, py, ROOM_EDGE.lerp(ROOM, open), look.room);
            }
            let spill = SPILL * field[(y * w + x) as usize];
            s.add(px, py, light([(look.neon, spill)]));
        }
    }
    if !layout.drawable() {
        return;
    }
    for (tube, _) in layout.tubes() {
        tube.paint_still(s);
    }
    for (_, x, count) in layout.separators() {
        for &share in lamp_heights(count) {
            bulb_still(s, layout.lamp(x, share), layout.tube_w);
        }
    }
}

/// Everything that moves: the glow, the lamps and the dust.
fn light_up(s: &mut Surface, layout: &Layout, look: Look, t: f64) {
    if layout.drawable() {
        let tubes = layout.tubes().count();
        for (i, (tube, digit)) in layout.tubes().enumerate() {
            if let Some(digit) = digit {
                tube.light_up(s, digit, brightness(i, tubes, t), look);
            }
        }
        for (i, x, count) in layout.separators() {
            let lit = 0.9 + 0.1 * noise(99 + i as u64, (t * 7.0) as u64);
            for &share in lamp_heights(count) {
                bulb_light(s, layout.lamp(x, share), layout.tube_w, lit, look);
            }
        }
    }
    dust(s, &layout.lights(), t, look);
}

/// The title, lit as the tubes are: its letters glowing neon, their light
/// spilling into the room around them, shimmering a little on its own.
fn caption_light(
    s: &mut Surface,
    font: &Font,
    title: &str,
    at: (f32, f32),
    width: u32,
    look: Look,
    t: f64,
) {
    let key = CaptionKey {
        title: title.to_string(),
        font: font_print(font),
        area: (s.area.width, s.area.height),
        at: (at.0.to_bits(), at.1.to_bits()),
        width,
    };
    let glow = kept(
        |kept| &mut kept.captions,
        key,
        32,
        || build_caption(font, title, (s.area.width, s.area.height), at, width),
    );
    let lit = 0.9 + 0.1 * noise(0x6361_7074, (t * 6.0) as u64);
    for &(x, y, core, halo) in glow.iter() {
        s.add(
            x,
            y,
            light([(look.neon, halo * lit), (look.hot, core * lit)]),
        );
    }
}

/// Where a title's neon reaches, in a `size` area with the title's middle
/// `at` its height and size: its letters, and a near and a far glow around
/// them.
fn build_caption(
    font: &Font,
    title: &str,
    size: (u32, u32),
    at: (f32, f32),
    width: u32,
) -> CaptionGlow {
    let (w, h) = size;
    // The letters, as coverage: white on black, drawn as any label is.
    let mut mask = Canvas::filled(w, h, Rgb::BLACK);
    draw::label(
        &mut mask,
        font,
        title,
        w as i32 / 2,
        at.0.round() as i32,
        at.1,
        Rgb::WHITE,
        Align::Center,
        width,
    );
    let letters: Vec<f32> = mask
        .as_rgb()
        .chunks(3)
        .map(|p| f32::from(p[0]) / 255.0)
        .collect();
    let near = blur(
        &letters,
        w as usize,
        h as usize,
        (at.1 * 0.1).max(1.0) as usize,
    );
    let far = blur(
        &letters,
        w as usize,
        h as usize,
        (at.1 * 0.35).max(2.0) as usize,
    );
    let mut out = Vec::new();
    for (i, ((&core, &near), &far)) in letters.iter().zip(&near).zip(&far).enumerate() {
        let (core, halo) = (core * 0.9, 0.6 * near + 0.3 * far);
        if core + halo > 0.004 {
            out.push(((i % w as usize) as i32, (i / w as usize) as i32, core, halo));
        }
    }
    out
}

/// `values`, a `w` by `h` grid, softened by a box `radius` either side, twice
/// over, which is near enough a gaussian for a glow.
fn blur(values: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let mut out = values.to_vec();
    for _ in 0..2 {
        out = box_pass(&out, w, h, radius, true);
        out = box_pass(&out, w, h, radius, false);
    }
    out
}

/// One pass of a box blur along the rows, or down the columns: a running
/// sum, so its cost does not grow with the radius.
fn box_pass(values: &[f32], w: usize, h: usize, radius: usize, across: bool) -> Vec<f32> {
    let (lines, length) = if across { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| if across { line * w + i } else { i * w + line };
    let span = (2 * radius + 1) as f32;
    let mut out = vec![0.0; values.len()];
    for line in 0..lines {
        let mut sum: f32 = (0..=radius.min(length.saturating_sub(1)))
            .map(|i| values[at(line, i)])
            .sum();
        for i in 0..length {
            out[at(line, i)] = sum / span;
            if i + radius + 1 < length {
                sum += values[at(line, i + radius + 1)];
            }
            if i >= radius {
                sum -= values[at(line, i - radius)];
            }
        }
    }
    out
}

/// Something that tells fonts apart, for keeping a title's glow: a font has
/// no name to go by, and a reload swaps it in place.
fn font_print(font: &Font) -> u64 {
    let width = font.measure("Hamburgefonstiv 0123", 64.0).to_bits();
    let ascent = font.ascent(64.0).to_bits();
    (u64::from(width) << 32) | u64::from(ascent)
}

#[derive(Clone, Copy)]
struct Tube {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    /// Whether it holds the symbols rather than the digits.
    symbols: bool,
}

impl Tube {
    /// The parts of the tube that never change: the glass, the cathodes
    /// nobody lit, the mesh in front of them, and the pip where the glass
    /// was sealed.
    fn paint_still(&self, s: &mut Surface) {
        let (w, h) = (self.w as f32, self.h as f32);
        let edge = |tx: i32, ty: i32| envelope(w, h, tx as f32 + 0.5, ty as f32 + 0.5);

        // The body, darkening the room behind it a little and the base, where
        // the pins go in, a lot; and the sheen of a curved sheet of glass,
        // down one side and on the dome.
        for ty in 0..self.h as i32 {
            for tx in 0..self.w as i32 {
                if edge(tx, ty) > 0.0 {
                    continue;
                }
                let (u, v) = ((tx as f32 + 0.5) / w, (ty as f32 + 0.5) / h);
                let (x, y) = (self.x + tx, self.y + ty);
                s.blend(x, y, GLASS_BODY, 0.3 + 0.5 * smoothstep(0.84, 0.98, v));
                let streak = (-((u - 0.2) / 0.05).powi(2)).exp()
                    * smoothstep(0.18, 0.3, v)
                    * (1.0 - smoothstep(0.7, 0.85, v));
                let (dx, dy) = ((u - 0.34) / 0.13, (ty as f32 + 0.5 - w * 0.2) / (w * 0.07));
                let dome = (-(dx * dx + dy * dy)).exp();
                s.blend(x, y, SHEEN, 0.07 * streak + 0.12 * dome);
            }
        }

        // The cathodes nobody lit, and the mesh in front of them.
        let at = Cathodes::of(self.w, self.h);
        let (span, _) = at.span();
        let (ox, oy) = at.origin();
        for (i, &wire) in cage(self.w, self.h, self.symbols).iter().enumerate() {
            if wire > 0.0 {
                let (sx, sy) = ((i as u32 % span) as i32, (i as u32 / span) as i32);
                s.blend(self.x + ox + sx, self.y + oy + sy, CAGE_METAL, 0.45 * wire);
            }
        }
        for ty in 0..self.h as i32 {
            for tx in 0..self.w as i32 {
                if mesh(self.w, self.h, tx, ty) > 0.0 && edge(tx, ty) < -2.0 {
                    s.blend(self.x + tx, self.y + ty, MESH_WIRE, 0.15);
                }
            }
        }

        // The rim, where the glass turns away and catches what light there is.
        for ty in -1..=self.h as i32 {
            for tx in -1..=self.w as i32 {
                let rim = (-((edge(tx, ty) + 0.6) / 0.9).powi(2)).exp();
                if rim > 0.02 {
                    s.blend(self.x + tx, self.y + ty, GLASS_RIM, 0.3 * rim);
                }
            }
        }

        let pip = (w * 0.05).max(1.0);
        let top = (self.x as f32 + w / 2.0, self.y as f32 - pip * 0.2);
        blend_ellipse(s, top, (pip, pip * 0.8), GLASS_RIM, 0.4);
    }

    /// The tube lit, showing `digit` at brightness `lit`: the wire and its
    /// glow, shadowed by the mesh and the other cathodes, and the light it
    /// throws back off its own glass.
    fn light_up(&self, s: &mut Surface, digit: u8, lit: f32, look: Look) {
        let at = Cathodes::of(self.w, self.h);
        let glow = glow(digit, self.w, self.h);
        let origin = (self.x + at.origin().0, self.y + at.origin().1);
        s.add_sprite(origin, at.span(), |i| {
            let [core, halo] = glow[i];
            light([(look.neon, halo * lit), (look.hot, core * lit)])
        });
        for &(tx, ty, strength) in rim(self.w, self.h).iter() {
            s.add(
                self.x + tx,
                self.y + ty,
                light([(look.neon, strength * lit)]),
            );
        }
    }
}

/// Where the digits sit in a tube: the box they are drawn in, from the
/// tube's top left, and how far past it their glow reaches.
#[derive(Clone, Copy)]
struct Cathodes {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    margin: u32,
}

impl Cathodes {
    fn of(w: u32, h: u32) -> Self {
        let (wf, hf) = (w as f32, h as f32);
        let box_w = (wf * 0.6).round().max(2.0) as u32;
        Self {
            x: ((wf - box_w as f32) / 2.0).round() as i32,
            y: (hf * 0.2).round() as i32,
            w: box_w,
            h: (hf * 0.62).round().max(2.0) as u32,
            margin: (wf * 0.35).round() as u32,
        }
    }

    /// How big a digit's sprite is: the box, and the glow around it.
    fn span(self) -> (u32, u32) {
        (self.w + self.margin * 2, self.h + self.margin * 2)
    }

    /// Where a digit's sprite starts, from the tube's top left.
    fn origin(self) -> (i32, i32) {
        (self.x - self.margin as i32, self.y - self.margin as i32)
    }
}

/// Whether a point of a `w` by `h` tube, from its top left, is on a wire of
/// the anode's mesh: a fine grid in front of the digits.
fn mesh(w: u32, h: u32, x: i32, y: i32) -> f32 {
    // Any smaller and it is a smudge rather than a grid.
    if w < 36 {
        return 0.0;
    }
    let pitch = (w as f32 / 14.0).round().max(4.0) as i32;
    let (left, right) = ((w as f32 * 0.12) as i32, (w as f32 * 0.88) as i32);
    let (top, bottom) = ((h as f32 * 0.13) as i32, (h as f32 * 0.9) as i32);
    let inside = (left..=right).contains(&x) && (top..=bottom).contains(&y);
    if inside && ((x - left) % pitch == 0 || (y - top) % pitch == 0) {
        1.0
    } else {
        0.0
    }
}

/// Signed distance from a point to the outline of a `w` by `h` tube, from
/// its top left: negative inside. A dome on top, nearly square at the base.
fn envelope(w: f32, h: f32, x: f32, y: f32) -> f32 {
    let (qx, qy) = ((x - w / 2.0).abs(), y - h / 2.0);
    let radius = if qy < 0.0 { w * 0.5 } else { w * 0.12 };
    let (dx, dy) = (qx - (w / 2.0 - radius), qy.abs() - (h / 2.0 - radius));
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0) - radius
}

/// A filled ellipse, smooth at its edge.
fn blend_ellipse(
    s: &mut Surface,
    (cx, cy): (f32, f32),
    (rx, ry): (f32, f32),
    color: Rgb,
    alpha: f32,
) {
    for y in (cy - ry - 1.0).floor() as i32..=(cy + ry + 1.0).ceil() as i32 {
        for x in (cx - rx - 1.0).floor() as i32..=(cx + rx + 1.0).ceil() as i32 {
            let (dx, dy) = ((x as f32 + 0.5 - cx) / rx, (y as f32 + 0.5 - cy) / ry);
            let edge = ((dx * dx + dy * dy).sqrt() - 1.0) * rx.min(ry);
            s.blend(x, y, color, alpha * (0.5 - edge).clamp(0.0, 1.0));
        }
    }
}

/// A separator lamp's glass bead, unlit, around the pixel `centre`.
fn bulb_still(s: &mut Surface, centre: (i32, i32), tube_w: f32) {
    let (cx, cy) = (centre.0 as f32 + 0.5, centre.1 as f32 + 0.5);
    let r = (tube_w * 0.075).max(1.5);
    let reach = (r + 2.0).ceil() as i32;
    for y in cy as i32 - reach..=cy as i32 + reach {
        for x in cx as i32 - reach..=cx as i32 + reach {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let d = (dx * dx + dy * dy).sqrt();
            if d < r {
                s.blend(x, y, GLASS_BODY, 0.45);
            }
            let rim = (-((d - r + 0.5) / 0.7).powi(2)).exp();
            s.blend(x, y, GLASS_RIM, 0.35 * rim);
            let (gx, gy) = (dx + r * 0.35, dy + r * 0.35);
            let glint = (-(gx * gx + gy * gy) / (r * r * 0.06)).exp();
            s.blend(x, y, SHEEN, 0.35 * glint);
        }
    }
}

/// A separator lamp lit: a glowing dot in its bead, dimmer than the tubes.
fn bulb_light(s: &mut Surface, (cx, cy): (i32, i32), tube_w: f32, lit: f32, look: Look) {
    let w = tube_w.round() as u32;
    let lamp = lamp(w);
    let r = lamp_reach(w);
    let side = (2 * r + 1) as u32;
    let lit = lit * 0.8;
    s.add_sprite((cx - r, cy - r), (side, side), |i| {
        let [solid, halo] = lamp[i];
        light([(look.neon, halo * lit), (look.hot, solid * lit)])
    });
}

/// How far a lamp's glow reaches beside tubes `w` wide, in whole pixels.
fn lamp_reach(w: u32) -> i32 {
    ((w as f32 * 0.13).max(2.0) * 2.5).ceil() as i32
}

/// A lamp's light, for lamps beside tubes `w` wide: per pixel of a square
/// centred on it, how much of its dot and of its halo reach there.
fn build_lamp(w: u32) -> Glow {
    let (core, reach) = ((w as f32 * 0.03).max(0.8), (w as f32 * 0.13).max(2.0));
    let r = lamp_reach(w);
    let mut out = Vec::with_capacity(((2 * r + 1) * (2 * r + 1)) as usize);
    for y in -r..=r {
        for x in -r..=r {
            let d = ((x * x + y * y) as f32).sqrt();
            let solid = (core + 0.5 - d).clamp(0.0, 1.0);
            let halo =
                0.5 * (-0.5 * (d / (reach * 0.35)).powi(2)).exp() + 0.18 * (-d / reach).exp();
            out.push([solid, halo]);
        }
    }
    out
}

/// Smoke drifting slowly through the room, seen where the tubes light it.
fn haze(s: &mut Surface, lights: &Lights, scale: f32, t: f64, look: Look) {
    // Worked out on a coarse grid and smoothed between: smoke has no edges
    // worth a noise sample per pixel.
    const CELL: u32 = 8;
    let (w, h) = (s.area.width, s.area.height);
    let (columns, rows) = (w / CELL + 2, h / CELL + 2);
    // Kept small before it is an f32, which cannot count seconds since 1970.
    let drift = (
        (t * 0.04).rem_euclid(4096.0) as f32,
        (t * 0.015).rem_euclid(4096.0) as f32,
    );
    let swirl = (t * 0.025).rem_euclid(4096.0) as f32;
    let mut grid = Vec::with_capacity((columns * rows) as usize);
    for gy in 0..rows {
        for gx in 0..columns {
            let (x, y) = ((gx * CELL) as f32, (gy * CELL) as f32);
            let (u, v) = (x / scale + drift.0, y / scale + drift.1);
            let n = 0.62 * value_noise(u, v, 11)
                + 0.38 * value_noise(u * 2.3 - swirl, v * 2.3 + 3.1, 23);
            grid.push(smoothstep(0.4, 0.88, n) * (0.35 + lights.at(x, y)) * HAZE_STRENGTH);
        }
    }
    let color = HAZE.lerp(look.neon, 0.35);
    let at = |column: usize, row: usize| grid[row * columns as usize + column];
    for y in 0..h {
        let (row, fy) = ((y / CELL) as usize, (y % CELL) as f32 / CELL as f32);
        for x in 0..w {
            let (column, fx) = ((x / CELL) as usize, (x % CELL) as f32 / CELL as f32);
            let top = at(column, row) + (at(column + 1, row) - at(column, row)) * fx;
            let bottom = at(column, row + 1) + (at(column + 1, row + 1) - at(column, row + 1)) * fx;
            s.add(
                x as i32,
                y as i32,
                light([(color, top + (bottom - top) * fy)]),
            );
        }
    }
}

/// Motes drifting slowly up through the light, seen only where the tubes
/// light them, and a few of them out of focus.
fn dust(s: &mut Surface, lights: &Lights, t: f64, look: Look) {
    use std::f64::consts::TAU;
    let (w, h) = (s.area.width as f64, s.area.height as f64);
    let count = ((w * h) / 3200.0).clamp(8.0, 50.0) as u64;
    let color = DUST.lerp(look.neon, 0.25);
    for i in 0..count {
        let u = |k: u64| unit(0xd057 + i * 16 + k);
        let speed = 0.4 + 0.6 * u(3);
        let x = (u(1) * w
            + t * (u(10) - 0.5) * w * 0.012
            + (t * 0.23 * speed + u(4) * TAU).sin() * w * 0.015)
            .rem_euclid(w) as f32;
        let y = (u(2) * h - t * speed * h * 0.012
            + (t * 0.37 * speed + u(5) * TAU).sin() * h * 0.04)
            .rem_euclid(h) as f32;
        let twinkle = (0.6 + 0.4 * (t * (0.5 + u(6) * 1.4) + u(7) * TAU).sin()) as f32;
        let seen = 0.03 + 0.97 * lights.at(x, y).min(1.0);
        let (radius, strength) = if u(9) > 0.88 {
            (2.2 + 2.0 * u(8) as f32, 0.2)
        } else {
            (0.55 + 0.9 * u(8) as f32, 0.7)
        };
        mote(s, (x, y), radius, color, strength * seen * twinkle);
    }
}

/// One mote: a point of light, or a soft disc if it is out of focus.
fn mote(s: &mut Surface, (cx, cy): (f32, f32), radius: f32, color: Rgb, strength: f32) {
    let reach = (radius + 1.5).ceil() as i32;
    for y in cy as i32 - reach..=cy as i32 + reach {
        for x in cx as i32 - reach..=cx as i32 + reach {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let cover = if radius < 1.5 {
                (radius + 0.5 - d).clamp(0.0, 1.0)
            } else {
                1.0 - smoothstep(radius * 0.5, radius, d)
            };
            s.add(x, y, light([(color, strength * cover)]));
        }
    }
}

/// How brightly tube `tube` of `tubes` burns at `t`: a faint shimmer
/// always, and now and then a stutter, as a tired cathode does.
fn brightness(tube: usize, tubes: usize, t: f64) -> f32 {
    let tube = tube as u64;
    let shimmer = 0.93 + 0.07 * noise(tube, (t * 9.0) as u64);
    let window = (t / FLICKER_WINDOW).floor();
    let roll = mix(window as u64 ^ 0x6e69_7869_6521);
    if roll.is_multiple_of(2) && (roll >> 8) % tubes.max(1) as u64 == tube {
        let start = unit(roll >> 16) * (FLICKER_WINDOW - 1.0);
        let length = 0.25 + unit(roll >> 24) * 0.55;
        let into = t - window * FLICKER_WINDOW - start;
        if (0.0..length).contains(&into) {
            let stutter = noise(tube, (t * 25.0) as u64);
            return shimmer
                * if stutter < 0.35 {
                    0.1
                } else {
                    0.4 + 0.6 * stutter
                };
        }
    }
    shimmer
}

/// What the still part of a tile's picture depends on.
#[derive(Clone, PartialEq, Eq, Hash)]
struct SceneKey {
    size: (u32, u32),
    glyphs: Vec<u8>,
    neon: (u8, u8, u8),
    caption: bool,
}

/// What a lit title depends on.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CaptionKey {
    title: String,
    font: u64,
    area: (u32, u32),
    /// Its middle's height and its size, as bits: floats do not hash.
    at: (u32, u32),
    width: u32,
}

/// What is kept between frames. Everything here is costly to work out and
/// depends only on sizes, which change when a layout is edited and not
/// otherwise; each map starts over when it fills, which only a daemon whose
/// layout is edited all day will see.
#[derive(Default)]
struct Kept {
    /// Every cathode's wire at once, per tube size and whether the tube
    /// holds symbols: the cathodes nobody lit.
    cages: HashMap<(u32, u32, bool), Rc<Vec<f32>>>,
    /// Per digit and tube size, how much of a lit wire's core and halo
    /// reaches each pixel past the mesh and the other cathodes.
    glows: HashMap<(u8, u32, u32), Rc<Glow>>,
    /// Per tube size, where its glass throws its own light back.
    rims: HashMap<(u32, u32), Rc<Rim>>,
    /// Per tube width, a separator lamp's light.
    lamps: HashMap<u32, Rc<Glow>>,
    /// Per tile, the pixels of everything that does not move.
    scenes: HashMap<SceneKey, Rc<Vec<u8>>>,
    /// Per tile, those with the haze over them, and which step of the
    /// haze that is.
    hazy: HashMap<SceneKey, (u64, Rc<Vec<u8>>)>,
    /// Per title, font and place, how much of its neon reaches each pixel.
    captions: HashMap<CaptionKey, Rc<CaptionGlow>>,
}

/// Per pixel of a digit's sprite, how much of a lit wire's core and halo
/// reaches it.
type Glow = Vec<[f32; 2]>;
/// Where a tube's glass throws its own light back: points from the tube's
/// top left, and how strongly.
type Rim = Vec<(i32, i32, f32)>;
/// Where a lit title's neon reaches: points in its area, and how much of the
/// letters' core and glow reaches each.
type CaptionGlow = Vec<(i32, i32, f32, f32)>;

thread_local! {
    static KEPT: RefCell<Kept> = RefCell::new(Kept::default());
}

/// `key` from the map `pick` chooses, made with `make` if it is not there.
/// Making one can need another, so it happens outside the borrow.
fn kept<K: Eq + Hash, V>(
    pick: fn(&mut Kept) -> &mut HashMap<K, Rc<V>>,
    key: K,
    limit: usize,
    make: impl FnOnce() -> V,
) -> Rc<V> {
    if let Some(found) = KEPT.with(|kept| pick(&mut kept.borrow_mut()).get(&key).cloned()) {
        return found;
    }
    let made = Rc::new(make());
    KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        let map = pick(&mut kept);
        if map.len() >= limit {
            map.clear();
        }
        map.insert(key, made.clone());
    });
    made
}

fn cage(w: u32, h: u32, symbols: bool) -> Rc<Vec<f32>> {
    kept(
        |kept| &mut kept.cages,
        (w, h, symbols),
        16,
        || build_cage(w, h, symbols),
    )
}

fn glow(digit: u8, w: u32, h: u32) -> Rc<Glow> {
    kept(
        |kept| &mut kept.glows,
        (digit, w, h),
        64,
        || build_glow(digit, w, h, &cage(w, h, is_symbol(digit))),
    )
}

fn lamp(w: u32) -> Rc<Glow> {
    kept(|kept| &mut kept.lamps, w, 16, || build_lamp(w))
}

fn rim(w: u32, h: u32) -> Rc<Rim> {
    kept(|kept| &mut kept.rims, (w, h), 16, || build_rim(w, h))
}

fn scene(key: SceneKey, make: impl FnOnce() -> Vec<u8>) -> Rc<Vec<u8>> {
    kept(|kept| &mut kept.scenes, key, 12, make)
}

/// A tile's still picture with the haze as it is at `epoch`, made again only
/// when the haze moves on.
fn hazy(key: SceneKey, epoch: f64, make: impl FnOnce() -> Vec<u8>) -> Rc<Vec<u8>> {
    let step = epoch.to_bits();
    let found = KEPT.with(|kept| {
        let kept = kept.borrow();
        let (at, pixels) = kept.hazy.get(&key)?;
        (*at == step).then(|| pixels.clone())
    });
    if let Some(found) = found {
        return found;
    }
    let made = Rc::new(make());
    KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        if kept.hazy.len() >= 12 && !kept.hazy.contains_key(&key) {
            kept.hazy.clear();
        }
        kept.hazy.insert(key, (step, made.clone()));
    });
    made
}

/// How much of every digit's wire covers each pixel of a digit's sprite.
fn build_cage(w: u32, h: u32, symbols: bool) -> Vec<f32> {
    let at = Cathodes::of(w, h);
    let (span, rows) = at.span();
    let mut out = vec![0.0f32; (span * rows) as usize];
    let all: Vec<Vec<(f32, f32)>> = if symbols {
        SYMBOLS.flat_map(strokes).collect()
    } else {
        (0..10).flat_map(strokes).collect()
    };
    // Each wire only reaches the pixels beside it, so each is drawn where
    // it is rather than every pixel measured against all three hundred.
    for (a, b) in wires(&all, at) {
        let columns = (a.0.min(b.0) - 2.0).max(0.0) as u32
            ..=(a.0.max(b.0) + 2.0).min(span as f32 - 1.0) as u32;
        let lines = (a.1.min(b.1) - 2.0).max(0.0) as u32
            ..=(a.1.max(b.1) + 2.0).min(rows as f32 - 1.0) as u32;
        for py in lines {
            for px in columns.clone() {
                let d = distance((px as f32 + 0.5, py as f32 + 0.5), a, b);
                let i = (py * span + px) as usize;
                out[i] = out[i].max((0.9 - d).clamp(0.0, 1.0));
            }
        }
    }
    out
}

/// A lit digit's light at each pixel of its sprite: the wire's core, and
/// the glow around it, both shadowed by the mesh in front, and the glow by
/// the other cathodes too.
fn build_glow(digit: u8, w: u32, h: u32, cage: &[f32]) -> Glow {
    let at = Cathodes::of(w, h);
    let (span, rows) = at.span();
    let (ox, oy) = at.origin();
    let segments = wires(&strokes(digit), at);
    let box_w = at.w as f32;
    let core_half = (box_w * 0.03).max(0.7);
    let spread = (box_w * 0.07).max(1.0);
    let bloom = (box_w * 0.16).max(1.5);
    let wide = (box_w * 0.45).max(3.0);
    let mut out = Vec::with_capacity((span * rows) as usize);
    for py in 0..rows {
        for px in 0..span {
            let p = (px as f32 + 0.5, py as f32 + 0.5);
            let d = segments
                .iter()
                .map(|&(a, b)| distance(p, a, b))
                .fold(f32::INFINITY, f32::min);
            let core = (core_half + 0.5 - d).clamp(0.0, 1.0);
            let halo = 0.7 * (-0.5 * (d / spread).powi(2)).exp()
                + 0.25 * (-d / bloom).exp()
                + 0.07 * (-d / wide).exp();
            let mesh = 1.0 - MESH_SHADOW * mesh(w, h, ox + px as i32, oy + py as i32);
            // A cathode's own wire does not shadow it.
            let others = 1.0 - CAGE_SHADOW * cage[(py * span + px) as usize] * (1.0 - core);
            out.push([core * mesh, halo * mesh * others]);
        }
    }
    out
}

/// Where a tube's glass throws its own light back, from its top left:
/// brightest down the sides beside the digit, more on one side than the
/// other as a curved sheet does, and a little on the dome.
fn build_rim(w: u32, h: u32) -> Rim {
    let (wf, hf) = (w as f32, h as f32);
    let mut out = Vec::new();
    for ty in 0..h as i32 {
        for tx in 0..w as i32 {
            let (x, y) = (tx as f32 + 0.5, ty as f32 + 0.5);
            let d = envelope(wf, hf, x, y);
            if !(-2.6..=0.4).contains(&d) {
                continue;
            }
            let band = (-((d + 1.0) / 0.8).powi(2)).exp();
            let side = (x - wf / 2.0) / (wf / 2.0);
            let beside = (-((y / hf - 0.52) / 0.24).powi(2)).exp();
            let sides = smoothstep(0.55, 0.92, side) + 0.5 * smoothstep(0.55, 0.92, -side);
            let dome = if y < wf * 0.5 {
                0.3 * smoothstep(0.0, 0.5, 1.0 - side.abs())
            } else {
                0.0
            };
            let strength = band * (sides * beside + dome) * 0.9;
            if strength > 0.004 {
                out.push((tx, ty, strength));
            }
        }
    }
    out
}

/// A digit's wires as segments, in the pixels of its sprite.
fn wires(strokes: &[Vec<(f32, f32)>], at: Cathodes) -> Vec<((f32, f32), (f32, f32))> {
    let (w, h, m) = (at.w as f32, at.h as f32, at.margin as f32);
    strokes
        .iter()
        .flat_map(|line| line.windows(2).map(|p| (p[0], p[1])))
        .map(|((ax, ay), (bx, by))| ((m + ax * w, m + ay * h), (m + bx * w, m + by * h)))
        .collect()
}

fn distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (abx, aby) = (b.0 - a.0, b.1 - a.1);
    let (apx, apy) = (p.0 - a.0, p.1 - a.1);
    let len = abx * abx + aby * aby;
    let t = if len > 0.0 {
        ((apx * abx + apy * aby) / len).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (dx, dy) = (apx - abx * t, apy - aby * t);
    (dx * dx + dy * dy).sqrt()
}

/// A digit's wires, in a unit box with y down.
fn strokes(digit: u8) -> Vec<Vec<(f32, f32)>> {
    match digit {
        0 => vec![arc((0.5, 0.5), (0.42, 0.5), 0.0, 360.0)],
        1 => vec![vec![(0.3, 0.14), (0.52, 0.0), (0.52, 1.0)]],
        2 => {
            let mut line = arc((0.5, 0.27), (0.4, 0.27), 180.0, 390.0);
            line.extend([(0.08, 1.0), (0.92, 1.0)]);
            vec![line]
        }
        3 => vec![
            arc((0.5, 0.25), (0.36, 0.25), 200.0, 450.0),
            arc((0.5, 0.74), (0.42, 0.26), 270.0, 520.0),
        ],
        4 => vec![vec![(0.72, 1.0), (0.72, 0.0), (0.06, 0.7), (0.96, 0.7)]],
        5 => {
            let mut line = vec![(0.86, 0.0), (0.22, 0.0), (0.16, 0.44)];
            line.extend(arc((0.5, 0.68), (0.4, 0.32), 215.0, 510.0));
            vec![line]
        }
        6 => vec![
            arc((0.5, 0.7), (0.4, 0.3), 0.0, 360.0),
            bezier((0.1, 0.7), (0.12, 0.08), (0.78, 0.02)),
        ],
        7 => vec![vec![(0.08, 0.0), (0.92, 0.0), (0.36, 1.0)]],
        8 => vec![
            arc((0.5, 0.24), (0.33, 0.24), 0.0, 360.0),
            arc((0.5, 0.72), (0.41, 0.28), 0.0, 360.0),
        ],
        // A six upside down, as on most real tubes.
        9 => strokes(6)
            .into_iter()
            .map(|line| line.into_iter().map(|(x, y)| (1.0 - x, 1.0 - y)).collect())
            .collect(),
        // The box is about half as wide as it is tall, so a circle is an
        // ellipse twice as wide here as it is high.
        PERCENT => vec![
            arc((0.24, 0.17), (0.2, 0.11), 0.0, 360.0),
            arc((0.76, 0.83), (0.2, 0.11), 0.0, 360.0),
            vec![(0.9, 0.0), (0.1, 1.0)],
        ],
        DEGREE => vec![degree_ring()],
        CELSIUS => vec![degree_ring(), arc((0.64, 0.6), (0.34, 0.4), 45.0, 315.0)],
        FAHRENHEIT => vec![
            degree_ring(),
            vec![(0.94, 0.2), (0.42, 0.2), (0.42, 1.0)],
            vec![(0.42, 0.58), (0.82, 0.58)],
        ],
        KILO => vec![
            vec![(0.16, 0.0), (0.16, 1.0)],
            vec![(0.88, 0.0), (0.16, 0.62)],
            vec![(0.4, 0.42), (0.9, 1.0)],
        ],
        MEGA => vec![vec![
            (0.06, 1.0),
            (0.06, 0.0),
            (0.5, 0.62),
            (0.94, 0.0),
            (0.94, 1.0),
        ]],
        GIGA => {
            let mut line = arc((0.5, 0.5), (0.42, 0.5), 315.0, 0.0);
            line.push((0.56, 0.5));
            vec![line]
        }
        PLUS => vec![vec![(0.5, 0.2), (0.5, 0.8)], vec![(0.1, 0.5), (0.9, 0.5)]],
        MINUS => vec![vec![(0.1, 0.5), (0.9, 0.5)]],
        _ => Vec::new(),
    }
}

/// The small ring of a degree sign, up in the top corner.
fn degree_ring() -> Vec<(f32, f32)> {
    arc((0.2, 0.12), (0.16, 0.09), 0.0, 360.0)
}

/// Points around an ellipse, in degrees clockwise from three o'clock.
fn arc(centre: (f32, f32), radius: (f32, f32), from: f32, to: f32) -> Vec<(f32, f32)> {
    let steps = ((to - from).abs() / 12.0).ceil().max(2.0) as usize;
    (0..=steps)
        .map(|i| {
            let a = (from + (to - from) * i as f32 / steps as f32).to_radians();
            (centre.0 + radius.0 * a.cos(), centre.1 + radius.1 * a.sin())
        })
        .collect()
}

fn bezier(a: (f32, f32), control: (f32, f32), b: (f32, f32)) -> Vec<(f32, f32)> {
    (0..=16)
        .map(|i| {
            let t = i as f32 / 16.0;
            let u = 1.0 - t;
            (
                u * u * a.0 + 2.0 * u * t * control.0 + t * t * b.0,
                u * u * a.1 + 2.0 * u * t * control.1 + t * t * b.1,
            )
        })
        .collect()
}

/// Colours of light at these strengths, together: per channel, how many
/// levels they add.
fn light<const N: usize>(parts: [(Rgb, f32); N]) -> [f32; 3] {
    let mut sum = [0.0; 3];
    for (color, amount) in parts {
        sum[0] += f32::from(color.r) * amount;
        sum[1] += f32::from(color.g) * amount;
        sum[2] += f32::from(color.b) * amount;
    }
    sum
}

/// Light added to a pixel's three channels. Near the top it rolls off rather
/// than clipping, as film does, which is what turns the middle of a wire
/// yellow instead of a flat orange.
fn lift(pixel: &mut [u8], light: [f32; 3]) {
    for (channel, by) in pixel.iter_mut().zip(light) {
        let base = f32::from(*channel);
        let sum = base + by;
        *channel = if sum <= KNEE {
            sum as u8
        } else {
            (base + roll_off(sum) - roll_off(base)).min(255.0) as u8
        };
    }
}

/// A level of light as it shows: itself up to a knee, then easing towards
/// white rather than stopping dead at it.
fn roll_off(level: f32) -> f32 {
    if level <= KNEE {
        level
    } else {
        KNEE + (255.0 - KNEE) * (1.0 - (-(level - KNEE) / (255.0 - KNEE)).exp())
    }
}

fn smoothstep(from: f32, to: f32, x: f32) -> f32 {
    let t = ((x - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Smooth noise: a random level at each whole coordinate, eased between.
fn value_noise(x: f32, y: f32, seed: u64) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = (x - fx, y - fy);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let corner = |i: f32, j: f32| {
        let (cx, cy) = ((fx + i) as i64 as u64, (fy + j) as i64 as u64);
        unit(seed ^ cx.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ cy.wrapping_mul(0xc2b2_ae3d_27d4_eb4f))
            as f32
    };
    let top = corner(0.0, 0.0) + (corner(1.0, 0.0) - corner(0.0, 0.0)) * sx;
    let bottom = corner(0.0, 1.0) + (corner(1.0, 1.0) - corner(0.0, 1.0)) * sx;
    top + (bottom - top) * sy
}

fn mix(mut x: u64) -> u64 {
    // splitmix64's finaliser.
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A number in 0..1 fixed by `seed`.
fn unit(seed: u64) -> f64 {
    (mix(seed) >> 11) as f64 / (1u64 << 53) as f64
}

fn noise(stream: u64, step: u64) -> f32 {
    unit(stream.wrapping_mul(0x1000_0001) ^ step) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck_model::{ColorRef, WidgetKind, WidgetView};

    const COLORS: Colors = Colors {
        background: Rgb::new(0, 0, 0),
        foreground: Rgb::new(240, 240, 240),
        accent: Rgb::new(40, 140, 255),
    };

    fn nixie() -> Widget {
        Widget {
            view: Some(WidgetView::Nixie),
            ..Widget::of(WidgetKind::Clock)
        }
    }

    fn frame_of(widget: &Widget, text: &str, t: f64) -> Canvas {
        let mut canvas = Canvas::filled(360, 120, Rgb::new(0, 0, 0));
        let area = Area::new(0, 0, 360, 120);
        draw(&mut canvas, area, widget, text, COLORS, None, t);
        canvas
    }

    fn frame(text: &str, t: f64) -> Canvas {
        frame_of(&nixie(), text, t)
    }

    fn glow_sum(canvas: &Canvas) -> u64 {
        canvas.as_rgb().chunks(3).map(|p| u64::from(p[0])).sum()
    }

    fn hottest(canvas: &Canvas) -> Rgb {
        let p = canvas
            .as_rgb()
            .chunks(3)
            .max_by_key(|p| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]))
            .unwrap();
        Rgb::new(p[0], p[1], p[2])
    }

    #[test]
    fn the_same_moment_draws_the_same_frame() {
        // The first draws what is kept and the second uses it.
        assert_eq!(
            frame("12:34:56", 10.0).as_rgb(),
            frame("12:34:56", 10.0).as_rgb()
        );
    }

    #[test]
    fn keeping_the_still_parts_changes_nothing() {
        let widget = nixie();
        let kept = frame_of(&widget, "12:34:56", 3.0);

        let mut drawn = Canvas::filled(360, 120, ROOM).into_rgb();
        let layout = Layout::new(360, 120, glyphs("12:34:56"), false);
        let look = Look::of(&widget, COLORS);
        let mut surface = Surface {
            pixels: &mut drawn,
            width: 360,
            height: 120,
            area: Area::new(0, 0, 360, 120),
        };
        // 3.0 is a whole step of the haze, so the haze is as it is at 3.0.
        paint_room(&mut surface, &layout, look, 3.0);
        light_up(&mut surface, &layout, look, 3.0);
        assert_eq!(kept.as_rgb(), &drawn[..]);
    }

    #[test]
    fn the_tubes_glow_orange() {
        let hot = hottest(&frame("12:34:56", 10.0));
        assert!(hot.r > 200 && hot.r > hot.b, "{hot:?}");
    }

    #[test]
    fn a_colour_of_its_own_replaces_the_orange() {
        let blue = Widget {
            color: Some(ColorRef::Literal(COLORS.accent)),
            ..nixie()
        };
        let hot = hottest(&frame_of(&blue, "12:34:56", 10.0));
        assert!(hot.b > hot.r, "{hot:?}");
    }

    #[test]
    fn the_room_is_dark_and_warm() {
        let corner = frame("12:34:56", 10.0).pixel(1, 1).unwrap();
        let level = u32::from(corner.r) + u32::from(corner.g) + u32::from(corner.b);
        assert!(level < 60 && corner.r >= corner.b, "{corner:?}");
    }

    #[test]
    fn the_air_moves() {
        // Same time on the clock, a moment later: the dust, the haze and the
        // shimmer differ, and they must.
        assert_ne!(
            frame("12:34:56", 10.0).as_rgb(),
            frame("12:34:56", 12.0).as_rgb()
        );
    }

    #[test]
    fn nothing_is_drawn_outside_the_area() {
        let mut canvas = Canvas::filled(400, 160, Rgb::new(1, 2, 3));
        let area = Area::new(20, 20, 360, 120);
        draw(&mut canvas, area, &nixie(), "88:88:88", COLORS, None, 10.0);
        for y in 0..160 {
            for x in 0..400 {
                let inside = (20..380).contains(&x) && (20..140).contains(&y);
                if !inside {
                    assert_eq!(canvas.pixel(x, y), Some(Rgb::new(1, 2, 3)), "at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn the_mesh_casts_a_shadow_across_the_glow() {
        let (w, h) = (96, 168);
        let at = Cathodes::of(w, h);
        let (span, rows) = at.span();
        let (ox, oy) = at.origin();
        // With no other cathodes in the way, only the mesh shadows the glow.
        let glow = build_glow(8, w, h, &vec![0.0; (span * rows) as usize]);
        let on_mesh = |sx: u32, sy: u32| mesh(w, h, ox + sx as i32, oy + sy as i32) > 0.0;
        // A row through the middle of the digit that is not itself a wire.
        let sy = (rows / 2..rows)
            .find(|&sy| (0..span).filter(|&sx| on_mesh(sx, sy)).count() < span as usize / 2)
            .unwrap();
        let mut crossings = 0;
        for sx in 0..span - 1 {
            let here = glow[(sy * span + sx) as usize][1];
            let next = glow[(sy * span + sx + 1) as usize][1];
            if on_mesh(sx, sy) && !on_mesh(sx + 1, sy) && next > 0.05 {
                assert!(here < next * 0.7, "{here} beside {next} at {sx}");
                crossings += 1;
            }
        }
        assert!(crossings > 2, "{crossings} wires crossed");
    }

    #[test]
    fn every_so_often_a_tube_flickers() {
        let dips = (0..6000)
            .map(|i| i as f64 * 0.05)
            .filter(|&t| (0..6).any(|tube| brightness(tube, 6, t) < 0.5))
            .count();
        assert!(dips > 0, "no tube flickered in five minutes");
        assert!(dips < 600, "tubes flicker too much: {dips} of 6000 frames");
    }

    #[test]
    fn a_one_lights_less_than_an_eight() {
        assert!(glow_sum(&frame("11:11:11", 0.0)) < glow_sum(&frame("88:88:88", 0.0)));
    }

    #[test]
    fn each_digit_is_a_tube_and_each_colon_a_pair_of_lamps() {
        use Glyph::{Separator, Tube};
        assert_eq!(
            glyphs("13:47:09"),
            [
                Tube(Some(1)),
                Tube(Some(3)),
                Separator(2),
                Tube(Some(4)),
                Tube(Some(7)),
                Separator(2),
                Tube(Some(0)),
                Tube(Some(9)),
            ]
        );
        assert_eq!(glyphs("13.47")[2], Separator(1));
    }

    #[test]
    fn padding_before_a_single_digit_is_a_dark_tube() {
        use Glyph::{Separator, Tube};
        // `%l:%M` at one o'clock.
        assert_eq!(
            glyphs(" 1:47"),
            [
                Tube(None),
                Tube(Some(1)),
                Separator(2),
                Tube(Some(4)),
                Tube(Some(7))
            ]
        );
        // `%a %l:%M`: the day is dropped, and the second space is padding.
        assert_eq!(glyphs("Sat  1:47"), glyphs(" 1:47"));
    }

    #[test]
    fn letters_are_left_out_and_so_are_the_spaces_around_them() {
        use Glyph::{Separator, Tube};
        // `%I:%M %p`.
        assert_eq!(glyphs("01:47 PM"), glyphs("01:47"));
        // A space after a word separates; it is not padding.
        assert_eq!(
            glyphs("13 h 47"),
            [
                Tube(Some(1)),
                Tube(Some(3)),
                Separator(0),
                Tube(Some(4)),
                Tube(Some(7))
            ]
        );
        assert!(glyphs("Saturday").is_empty());
    }

    #[test]
    fn nothing_to_show_or_nowhere_to_show_it_draws_no_tubes_and_does_not_panic() {
        let dark = frame("Saturday", 0.0);
        assert!(hottest(&dark).r < 200, "no tube should be lit");
        let mut canvas = Canvas::filled(10, 10, Rgb::new(0, 0, 0));
        let area = Area::new(0, 0, 10, 10);
        draw(&mut canvas, area, &nixie(), "12:34:56", COLORS, None, 0.0);
    }

    #[test]
    fn a_reading_s_unit_goes_on_a_symbol_tube() {
        use Glyph::{Separator, Tube};
        assert_eq!(
            glyphs("42%"),
            [Tube(Some(4)), Tube(Some(2)), Tube(Some(PERCENT))]
        );
        assert_eq!(
            glyphs("63°C"),
            [Tube(Some(6)), Tube(Some(3)), Tube(Some(CELSIUS))]
        );
        assert_eq!(
            glyphs("-5°F"),
            [Tube(Some(MINUS)), Tube(Some(5)), Tube(Some(FAHRENHEIT))]
        );
        // A network rate: the arrows are gaps, `K` is a tube, and bytes have
        // no symbol.
        assert_eq!(
            glyphs("↓2.0K ↑850B"),
            [
                Tube(Some(2)),
                Separator(1),
                Tube(Some(0)),
                Tube(Some(KILO)),
                Separator(0),
                Tube(Some(8)),
                Tube(Some(5)),
                Tube(Some(0)),
            ]
        );
        assert_eq!(glyphs("12M")[2], Tube(Some(MEGA)));
        assert_eq!(glyphs("1.5G")[3], Tube(Some(GIGA)));
    }

    #[test]
    fn a_sign_leads_a_number_and_a_unit_follows_one() {
        use Glyph::{Separator, Tube};
        assert_eq!(glyphs("+3"), [Tube(Some(PLUS)), Tube(Some(3))]);
        // The dashes of a date are gaps, not signs.
        let date = glyphs("2026-09-26");
        assert!(!date.contains(&Tube(Some(MINUS))));
        assert_eq!(date[4], Separator(0));
        // The `M` of a meridiem is a letter, not mega.
        assert_eq!(glyphs("11:05 PM"), glyphs("11:05"));
        assert_eq!(glyphs("Mon 11:05"), glyphs("11:05"));
    }

    #[test]
    fn every_symbol_is_wire_inside_its_box() {
        for symbol in SYMBOLS {
            let lines = strokes(symbol);
            assert!(!lines.is_empty(), "symbol {symbol} has no wire");
            for &(x, y) in lines.iter().flatten() {
                assert!(
                    (-0.01..=1.01).contains(&x) && (-0.01..=1.01).contains(&y),
                    "symbol {symbol} strays to ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn a_symbol_tube_holds_symbols_behind_its_glass() {
        let layout = |text: &str| Layout::new(360, 120, glyphs(text), false);
        // Kept apart: a symbol tube's still picture is not a digit tube's.
        assert_ne!(layout("44").signature(), layout("4%").signature());
        let symbols: Vec<bool> = layout("4%").tubes().map(|(tube, _)| tube.symbols).collect();
        assert_eq!(symbols, [false, true]);
    }

    #[test]
    fn a_symbol_lights_its_tube() {
        assert!(glow_sum(&frame("42%", 10.0)) > glow_sum(&frame("42", 10.0)));
    }

    #[test]
    fn the_title_glows_as_neon_does() {
        // A machine with no font draws no title at all.
        let Some(font) = Font::system() else {
            return;
        };
        let (w, h) = (360, 120);
        let mut pixels = vec![0u8; (w * h * 3) as usize];
        let mut s = Surface {
            pixels: &mut pixels,
            width: w,
            height: h,
            area: Area::new(0, 0, w, h),
        };
        let look = Look {
            neon: NEON,
            hot: HOT,
            room: 1.0,
        };
        caption_light(&mut s, &font, "CPU", (100.0, 20.0), w, look, 0.0);
        let canvas = Canvas::from_rgb(w, h, pixels).unwrap();
        let hot = hottest(&canvas);
        assert!(hot.r >= 200 && hot.r >= hot.g && hot.g >= hot.b, "{hot:?}");
        // The glow stays about the letters: the top of the tile is dark.
        let top: u64 = canvas.as_rgb()[..(w * 40 * 3) as usize]
            .iter()
            .map(|&c| u64::from(c))
            .sum();
        assert_eq!(top, 0);
    }
}
