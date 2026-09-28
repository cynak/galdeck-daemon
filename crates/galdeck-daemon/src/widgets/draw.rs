//! Drawing a widget's reading into a rectangle.
//!
//! The same code draws a key and a tile on the info screen; only the
//! rectangle and the colours differ. Layouts are proportional to the
//! rectangle rather than fixed in pixels, so a graph looks the same on a
//! 160px key and on a quarter of the screen.
//!
//! Weather icons and media glyphs are drawn from primitives rather than
//! shipped as images. They are a dozen shapes, and drawing them means they
//! scale to any tile without an asset pipeline or a licence to track.

use galdeck::{Align, Canvas, Font, Rgb, TextStyle};
use galdeck_model::{BarStyle, GraphStyle, Widget, WidgetKind, WidgetView};

use super::media::{self, Media, Status};
use super::weather::{Condition, Weather};
use super::{Reading, SlotState};

/// A rectangle on a canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Area {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn inset(self, by: u32) -> Self {
        Self {
            x: self.x + by as i32,
            y: self.y + by as i32,
            width: self.width.saturating_sub(by * 2),
            height: self.height.saturating_sub(by * 2),
        }
    }

    fn right(self) -> i32 {
        self.x + self.width as i32
    }

    fn bottom(self) -> i32 {
        self.y + self.height as i32
    }

    fn centre_x(self) -> i32 {
        self.x + self.width as i32 / 2
    }

    fn centre_y(self) -> i32 {
        self.y + self.height as i32 / 2
    }

    /// A share of the height, in pixels, for sizing text and padding.
    fn h(self, share: f32) -> f32 {
        self.height as f32 * share
    }
}

/// The colours a widget is drawn in, already resolved through the theme.
#[derive(Clone, Copy, Debug)]
pub struct Colors {
    /// Behind everything.
    pub background: Rgb,
    /// Text.
    pub foreground: Rgb,
    /// The graph line, the bar, the progress bar.
    pub accent: Rgb,
}

impl Colors {
    /// Secondary text: the foreground, faded towards the background.
    fn muted(self) -> Rgb {
        self.foreground.lerp(self.background, 0.4)
    }

    /// Everything but the background faded halfway into it, for a timer
    /// that is paused: still readable, but plainly not counting.
    pub fn dimmed(self) -> Self {
        Self {
            background: self.background,
            foreground: self.foreground.lerp(self.background, 0.5),
            accent: self.accent.lerp(self.background, 0.5),
        }
    }
}

// Icon colours are fixed rather than themed: a sun is yellow and rain is blue
// in every theme, and a monochrome weather icon is much harder to read.
const SUN: Rgb = Rgb::new(255, 200, 64);
const MOON: Rgb = Rgb::new(230, 232, 240);
const CLOUD: Rgb = Rgb::new(214, 222, 235);
const STORM_CLOUD: Rgb = Rgb::new(150, 158, 175);
const RAIN: Rgb = Rgb::new(96, 170, 255);
const SNOW: Rgb = Rgb::new(245, 248, 255);
const BOLT: Rgb = Rgb::new(255, 214, 10);

/// Draw a widget's reading into `area`.
///
/// `fallback` is what to show when there is no reading yet: the widget's
/// placeholder, or the key's label.
pub fn widget(
    canvas: &mut Canvas,
    area: Area,
    widget: &Widget,
    state: Option<&SlotState>,
    colors: Colors,
    font: Option<&Font>,
    fallback: Option<&str>,
) {
    let reading = state.and_then(|s| s.reading.as_ref());
    match (widget.kind, reading) {
        (WidgetKind::Weather, Some(Reading::Weather(weather))) => {
            weather_card(canvas, area, weather, colors, font)
        }
        (WidgetKind::Media, Some(Reading::Media(media))) => {
            media_card(canvas, area, media.as_ref(), colors, font)
        }
        _ => match widget.view() {
            // Weather and media draw as cards whatever `view` says; with no
            // reading yet they fall through to here and show the fallback.
            // A timer fills a bar or a gauge with what it has left.
            view @ WidgetView::Graph if view.suits(widget.kind) => {
                graph(canvas, area, widget, state, colors, font, fallback)
            }
            view @ WidgetView::Bar if view.suits(widget.kind) => {
                bar(canvas, area, widget, state, colors, font, fallback)
            }
            view @ WidgetView::Gauge if view.suits(widget.kind) => {
                gauge(canvas, area, widget, state, colors, font, fallback)
            }
            WidgetView::Analog if widget.kind == WidgetKind::Clock => {
                let seconds = reading.and_then(Reading::value);
                analog(canvas, area, widget.title.as_deref(), seconds, colors, font)
            }
            view @ WidgetView::Nixie if view.suits(widget.kind) => {
                // A tube for each digit: of the formatted time for a clock, so
                // `format` decides how many there are, and of the reading for
                // anything else, its unit on a symbol tube. Before the first
                // reading there is only the title, lit.
                let text = reading.and_then(Reading::label).unwrap_or_else(|| {
                    if widget.kind == WidgetKind::Clock {
                        super::now_in(widget.timezone())
                            .strftime(widget.format())
                            .to_string()
                    } else {
                        String::new()
                    }
                });
                let t = super::nixie::now();
                super::nixie::draw(canvas, area, widget, &text, colors, font, t)
            }
            _ => {
                let text = reading
                    .and_then(Reading::label)
                    .or_else(|| fallback.map(str::to_string))
                    .unwrap_or_default();
                text_tile(canvas, area, widget.title.as_deref(), &text, colors, font)
            }
        },
    }
}

/// A caption, if any, over the largest text that fits.
fn text_tile(
    canvas: &mut Canvas,
    area: Area,
    title: Option<&str>,
    text: &str,
    colors: Colors,
    font: Option<&Font>,
) {
    let Some(font) = font else { return };
    let area = area.inset((area.height / 12).max(4));
    let mut body = area;
    if let Some(title) = title {
        let size = area.h(0.18).min(28.0);
        label(
            canvas,
            font,
            title,
            area.x,
            area.y + (size * 0.6) as i32,
            size,
            colors.muted(),
            Align::Left,
            area.width,
        );
        body.y += (size * 1.2) as i32;
        body.height = body.height.saturating_sub((size * 1.2) as u32);
    }
    let size = font
        .fitting_size(text, body.width as f32, body.h(0.72))
        .max(8.0);
    label(
        canvas,
        font,
        text,
        body.centre_x(),
        body.centre_y(),
        size,
        colors.foreground,
        Align::Center,
        body.width,
    );
}

/// A filled graph of recent readings, with the caption and latest text over
/// it — the shape of the CPU and GPU keys people put on a deck.
fn graph(
    canvas: &mut Canvas,
    area: Area,
    widget: &Widget,
    state: Option<&SlotState>,
    colors: Colors,
    font: Option<&Font>,
    fallback: Option<&str>,
) {
    if let Some(state) = state.filter(|s| !s.history.is_empty()) {
        // The plot takes the lower part, so the text above it stays legible.
        let plot = Area::new(
            area.x,
            area.y + area.h(0.4) as i32,
            area.width,
            (area.height as f32 * 0.6) as u32,
        );
        plot_history(
            canvas,
            plot,
            &state.history,
            widget.history(),
            state.scale(widget),
            colors.accent,
            widget.graph_style(),
        );
    }
    captioned_value(
        canvas, area, widget, state, colors, font, fallback, 0.2, 0.42,
    );
}

/// A bar filled to the latest reading.
fn bar(
    canvas: &mut Canvas,
    area: Area,
    widget: &Widget,
    state: Option<&SlotState>,
    colors: Colors,
    font: Option<&Font>,
    fallback: Option<&str>,
) {
    let pad = (area.height / 10).max(4);
    let thickness = (area.height / 8).max(4);
    let track = Area::new(
        area.x + pad as i32,
        area.bottom() - (pad + thickness) as i32,
        area.width.saturating_sub(pad * 2),
        thickness,
    );
    let value = state.and_then(|s| s.reading.as_ref()?.value().map(|v| (v, s.scale(widget))));
    let share = value.map(|(value, scale)| (value / scale).clamp(0.0, 1.0));
    match widget.bar_style() {
        BarStyle::Segmented => segmented_bar(canvas, track, share, widget.segments(), colors),
        style => {
            let radius = if style == BarStyle::Rounded {
                thickness / 2
            } else {
                0
            };
            // Translucent rather than a mixed colour, so the track reads the
            // same over a picture as over a flat background.
            blend_round_rect(canvas, track, radius, colors.foreground, 0.2);
            if let Some(share) = share.filter(|share| *share > 0.0) {
                let filled = Area {
                    width: ((track.width as f64 * share).round() as u32).max(thickness),
                    ..track
                };
                fill_round_rect(canvas, filled, radius, colors.accent);
            }
        }
    }
    let above = Area {
        height: area.height.saturating_sub(pad + thickness),
        ..area
    };
    captioned_value(
        canvas, above, widget, state, colors, font, fallback, 0.24, 0.5,
    );
}

/// A bar lit a step at a time, like a level meter: `segments` blocks with a
/// gap between, as many lit as the reading reaches, to the nearest.
fn segmented_bar(
    canvas: &mut Canvas,
    track: Area,
    share: Option<f64>,
    segments: u8,
    colors: Colors,
) {
    let count = u32::from(segments.max(1));
    let gap = (track.height / 3).clamp(1, 4);
    let width = track.width.saturating_sub(gap * (count - 1)) / count;
    if width == 0 {
        return;
    }
    // What the blocks leave over goes at the ends, so the meter is centred.
    let spare = track.width - (width * count + gap * (count - 1));
    let lit = share.map_or(0, |share| (share * f64::from(count)).round() as u32);
    for index in 0..count {
        let block = Area::new(
            track.x + (spare / 2 + index * (width + gap)) as i32,
            track.y,
            width,
            track.height,
        );
        if index < lit {
            fill_round_rect(canvas, block, 1, colors.accent);
        } else {
            blend_round_rect(canvas, block, 1, colors.foreground, 0.2);
        }
    }
}

/// The caption at the top and the reading's text below it.
#[allow(clippy::too_many_arguments)]
fn captioned_value(
    canvas: &mut Canvas,
    area: Area,
    widget: &Widget,
    state: Option<&SlotState>,
    colors: Colors,
    font: Option<&Font>,
    fallback: Option<&str>,
    title_share: f32,
    value_at: f32,
) {
    let Some(font) = font else { return };
    let inner = area.inset((area.height / 14).max(4));
    let title = widget
        .title
        .clone()
        .unwrap_or_else(|| default_title(widget.kind).to_string());
    let title_size = inner.h(title_share).min(40.0);
    label(
        canvas,
        font,
        &title,
        inner.centre_x(),
        inner.y + (title_size * 0.6) as i32,
        title_size,
        colors.foreground,
        Align::Center,
        inner.width,
    );

    let text = state
        .and_then(|s| s.reading.as_ref())
        .and_then(Reading::label)
        .or_else(|| fallback.map(str::to_string))
        .unwrap_or_default();
    let size = font.fitting_size(&text, inner.width as f32, inner.h(0.26).min(72.0));
    label(
        canvas,
        font,
        &text,
        inner.centre_x(),
        inner.y + inner.h(value_at) as i32,
        size,
        colors.foreground,
        Align::Center,
        inner.width,
    );
}

fn default_title(kind: WidgetKind) -> &'static str {
    match kind {
        WidgetKind::Battery => "Battery",
        WidgetKind::Fan => "Fan",
        WidgetKind::Load => "Load",
        WidgetKind::Volume => "Volume",
        WidgetKind::Cpu => "CPU",
        WidgetKind::Memory => "RAM",
        WidgetKind::Gpu => "GPU",
        WidgetKind::Temperature => "Temp",
        WidgetKind::Network => "Net",
        WidgetKind::Disk => "Disk",
        WidgetKind::Timer => "Timer",
        _ => "",
    }
}

/// Newest reading at the right edge, older ones trailing off to the left:
/// a line with the area under it filled in a faded accent, the line alone,
/// or columns, as `style` says.
fn plot_history(
    canvas: &mut Canvas,
    plot: Area,
    history: &std::collections::VecDeque<f64>,
    capacity: usize,
    scale: f64,
    accent: Rgb,
    style: GraphStyle,
) {
    if style == GraphStyle::Bars {
        plot_columns(canvas, plot, history, capacity, scale, accent);
        return;
    }
    let filled = style == GraphStyle::Area;
    let thickness = (plot.height / 40).clamp(2, 4);
    // The line's brush is a square centred on it, so the line itself keeps a
    // brush's width inside the plot; otherwise a full graph paints a pixel
    // or two into whatever is beside it.
    let bottom = plot.bottom();
    let plot = Area::new(
        plot.x + thickness as i32,
        plot.y + thickness as i32,
        plot.width.saturating_sub(thickness * 2),
        plot.height.saturating_sub(thickness),
    );
    if plot.width < 2 || plot.height < 2 {
        return;
    }
    let step = (plot.width - 1) as f64 / (capacity.max(2) - 1) as f64;
    let newest = history.len() - 1;
    let point = |i: usize| -> (f64, f64) {
        let x = plot.right() as f64 - 1.0 - (newest - i) as f64 * step;
        let share = (history[i] / scale).clamp(0.0, 1.0);
        let y = plot.bottom() as f64 - 1.0 - share * (plot.height - 1) as f64;
        (x, y)
    };
    // Blended rather than mixed with the background colour, so the area
    // under the line is a tint over whatever is there -- a picture included.
    let fill = |canvas: &mut Canvas, x: i32, from: f64| {
        for y in from.max(0.0) as i32..bottom {
            canvas.blend_pixel(x, y, accent, 0.35);
        }
    };

    if history.len() == 1 {
        let (x, y) = point(0);
        if filled {
            fill(canvas, x as i32, y);
        } else {
            // One reading is a point, and a point needs some width to see.
            let from = (x as i32 - thickness as i32 * 2).max(plot.x);
            canvas.draw_line_thick((from, y as i32), (x as i32, y as i32), accent, thickness);
        }
        return;
    }
    // Column by column: the line's height at each pixel, interpolated between
    // the two readings either side of it.
    let first = point(0).0.max(plot.x as f64).ceil() as i32;
    let mut segment = 0;
    for px in (first..plot.right()).filter(|_| filled) {
        let x = px as f64;
        while segment + 1 < newest && point(segment + 1).0 < x {
            segment += 1;
        }
        let (x0, y0) = point(segment);
        let (x1, y1) = point(segment + 1);
        let t = if x1 > x0 {
            ((x - x0) / (x1 - x0)).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let y = y0 + (y1 - y0) * t;
        fill(canvas, px, y);
    }
    for i in 0..newest {
        let (x0, y0) = point(i);
        let (x1, y1) = point(i + 1);
        if x1 < plot.x as f64 {
            continue;
        }
        canvas.draw_line_thick(
            (x0.max(plot.x as f64) as i32, y0 as i32),
            (x1 as i32, y1 as i32),
            accent,
            thickness,
        );
    }
}

/// Readings as columns with a gap between, newest on the right. There are
/// fewer columns than readings on anything but a wide tile, so each stands
/// for a stretch of them and shows its highest: a spike is not averaged
/// away. The stretches are fixed across the whole history, so a column does
/// not change what it stands for as the history fills.
fn plot_columns(
    canvas: &mut Canvas,
    plot: Area,
    history: &std::collections::VecDeque<f64>,
    capacity: usize,
    scale: f64,
    accent: Rgb,
) {
    let capacity = capacity.max(history.len()).max(1);
    let gap = (plot.width / 48).max(1);
    let columns = (plot.width / (3 + gap)).clamp(1, capacity as u32);
    let width = plot.width.saturating_sub(gap * (columns - 1)) / columns;
    if width == 0 || plot.height == 0 {
        return;
    }
    let spare = plot.width - (width * columns + gap * (columns - 1));
    // Readings missing from the start of a history that is not full yet.
    let missing = capacity - history.len();
    let per = capacity as f64 / f64::from(columns);
    for column in 0..columns {
        let from = (f64::from(column) * per).floor() as usize;
        let to = ((f64::from(column + 1) * per).ceil() as usize).clamp(from + 1, capacity);
        let highest = (from..to)
            .filter_map(|index| history.get(index.checked_sub(missing)?))
            .copied()
            .reduce(f64::max);
        let Some(value) = highest else {
            continue;
        };
        let height = ((value / scale).clamp(0.0, 1.0) * f64::from(plot.height)).round() as u32;
        if height == 0 {
            continue;
        }
        let bar = Area::new(
            plot.x + (spare + column * (width + gap)) as i32,
            plot.bottom() - height as i32,
            width,
            height,
        );
        fill_round_rect(canvas, bar, (width / 3).min(2), accent);
    }
}

/// A dial: an arc open at the bottom -- 270 degrees unless the widget or its
/// look says otherwise -- filled to the reading, with the reading written
/// inside and the caption below.
fn gauge(
    canvas: &mut Canvas,
    area: Area,
    widget: &Widget,
    state: Option<&SlotState>,
    colors: Colors,
    font: Option<&Font>,
    fallback: Option<&str>,
) {
    let sweep = widget.sweep();
    // Whatever the dial leaves open is at the bottom, centred.
    let open = 360.0 - sweep;
    let start = 90.0 + open / 2.0;
    let inner = area.inset((area.height / 14).max(3));
    let radius = inner.width.min(inner.height) as f32 * 0.5;
    let thickness = (radius * widget.thickness()).max(3.0);
    let middle = radius - thickness / 2.0;
    let (cx, cy) = (
        inner.centre_x() as f32,
        // Nudged down by part of what the open bottom leaves empty, so the
        // top does not look crowded: none for a full ring, a sixteenth of the
        // radius for the usual three-quarters.
        inner.centre_y() as f32 + radius * (1.0 - (open / 2.0).to_radians().cos()) * 0.205,
    );
    // Where the reading and the caption go: in the gap at the bottom, or
    // inside a ring that has none, or under an arch.
    let (value_y, title_y) = if open < 60.0 {
        (cy - radius * 0.08, cy + radius * 0.34)
    } else if open > 120.0 {
        (cy - radius * 0.18, cy + radius * 0.36)
    } else {
        (cy, cy + radius * 0.62)
    };
    let share = state
        .and_then(|s| Some((s.reading.as_ref()?.value()?, s.scale(widget))))
        .map_or(0.0, |(value, scale)| (value / scale).clamp(0.0, 1.0) as f32);

    let reach = radius.ceil() as i32 + 1;
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let (x, y) = (dx as f32, dy as f32);
            let distance = x.hypot(y);
            // Coverage across the ring's width, soft at both edges.
            let coverage = (thickness / 2.0 - (distance - middle).abs() + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            // Clockwise from the start, in screen coordinates.
            let angle = (y.atan2(x).to_degrees() - start).rem_euclid(360.0);
            if angle > sweep {
                continue;
            }
            let (px, py) = (cx as i32 + dx, cy as i32 + dy);
            if angle <= sweep * share {
                canvas.blend_pixel(px, py, colors.accent, coverage);
            } else {
                canvas.blend_pixel(px, py, colors.foreground, coverage * 0.18);
            }
        }
    }

    let Some(font) = font else { return };
    let text = state
        .and_then(|s| s.reading.as_ref())
        .and_then(Reading::label)
        .or_else(|| fallback.map(str::to_string))
        .unwrap_or_default();
    let width = (middle * 1.5) as u32;
    let size = font.fitting_size(&text, width as f32, radius * 0.42);
    label(
        canvas,
        font,
        &text,
        cx as i32,
        value_y as i32,
        size,
        colors.foreground,
        Align::Center,
        width,
    );
    let title = widget
        .title
        .clone()
        .unwrap_or_else(|| default_title(widget.kind).to_string());
    let size = font.fitting_size(&title, width as f32, radius * 0.2);
    label(
        canvas,
        font,
        &title,
        cx as i32,
        title_y as i32,
        size,
        colors.muted(),
        Align::Center,
        width,
    );
}

/// A clock face: ticks, three hands, the second hand in the accent.
///
/// `seconds` is the time of day in the widget's zone; without one yet, the
/// local time now, so the face is never blank.
fn analog(
    canvas: &mut Canvas,
    area: Area,
    title: Option<&str>,
    seconds: Option<f64>,
    colors: Colors,
    font: Option<&Font>,
) {
    let seconds = seconds.unwrap_or_else(|| {
        let time = jiff::Zoned::now().time();
        f64::from(time.hour()) * 3600.0 + f64::from(time.minute()) * 60.0 + f64::from(time.second())
    }) as f32;
    let inner = area.inset((area.height / 12).max(3));
    let radius = inner.width.min(inner.height) as f32 * 0.5;
    let (cx, cy) = (inner.centre_x(), inner.centre_y());
    let at = |angle: f32, length: f32| {
        let radians = angle.to_radians();
        (
            cx + (radians.sin() * length) as i32,
            cy - (radians.cos() * length) as i32,
        )
    };

    blend_disc(canvas, (cx, cy), radius as u32, colors.foreground, 0.06);
    for hour in 0..12 {
        let angle = hour as f32 * 30.0;
        let major = hour % 3 == 0;
        let inset = if major { 0.16 } else { 0.09 };
        let thickness = if major {
            (radius * 0.05).max(2.0)
        } else {
            (radius * 0.025).max(1.0)
        };
        canvas.draw_line_thick(
            at(angle, radius * (0.96 - inset)),
            at(angle, radius * 0.96),
            colors
                .foreground
                .lerp(colors.background, if major { 0.1 } else { 0.4 }),
            thickness as u32,
        );
    }

    let (hours, minutes, secs) = (
        (seconds / 3600.0) % 12.0,
        (seconds / 60.0) % 60.0,
        seconds % 60.0,
    );
    canvas.draw_line_thick(
        (cx, cy),
        at(hours * 30.0, radius * 0.5),
        colors.foreground,
        (radius * 0.07).max(2.0) as u32,
    );
    canvas.draw_line_thick(
        (cx, cy),
        at(minutes * 6.0, radius * 0.78),
        colors.foreground,
        (radius * 0.045).max(2.0) as u32,
    );
    // A short tail behind the pivot, as a real second hand has.
    canvas.draw_line_thick(
        at(secs.floor() * 6.0 + 180.0, radius * 0.15),
        at(secs.floor() * 6.0, radius * 0.86),
        colors.accent,
        (radius * 0.02).max(1.0) as u32,
    );
    canvas.fill_circle((cx, cy), (radius * 0.05).max(2.0) as u32, colors.accent);

    if let (Some(title), Some(font)) = (title, font) {
        let size = (radius * 0.16).max(9.0);
        label(
            canvas,
            font,
            title,
            cx,
            cy + (radius * 0.42) as i32,
            size,
            colors.muted(),
            Align::Center,
            (radius * 1.2) as u32,
        );
    }
}

/// Current conditions, and — where there is room — the days ahead.
fn weather_card(
    canvas: &mut Canvas,
    area: Area,
    weather: &Weather,
    colors: Colors,
    font: Option<&Font>,
) {
    let Some(font) = font else { return };
    let pad = (area.height / 12).max(4);
    let inner = area.inset(pad);
    let wide = inner.width as f32 >= inner.height as f32 * 1.6 && weather.days.len() > 1;

    // Today, in a square at the left (or the whole tile when it is small).
    let today = if wide {
        Area {
            width: inner.height.min(inner.width / 2),
            ..inner
        }
    } else {
        inner
    };
    let icon_size = today.h(0.5) as u32;
    icon(
        canvas,
        weather.condition,
        weather.is_day,
        (today.centre_x(), today.y + today.h(0.28) as i32),
        icon_size,
        colors.background,
    );
    let temperature = Weather::degrees(weather.temperature);
    let size = font.fitting_size(&temperature, today.width as f32, today.h(0.3));
    label(
        canvas,
        font,
        &temperature,
        today.centre_x(),
        today.y + today.h(0.7) as i32,
        size,
        colors.foreground,
        Align::Center,
        today.width,
    );
    if let Some(day) = weather.days.first() {
        let range = format!(
            "{} / {}",
            Weather::degrees(day.high),
            Weather::degrees(day.low)
        );
        let size = font.fitting_size(&range, today.width as f32, today.h(0.13));
        label(
            canvas,
            font,
            &range,
            today.centre_x(),
            today.y + today.h(0.91) as i32,
            size,
            colors.muted(),
            Align::Center,
            today.width,
        );
    }
    if !wide {
        return;
    }

    // The days after, in equal columns across the rest.
    let rest = Area::new(
        today.right() + pad as i32,
        inner.y,
        (inner.right() - today.right() - pad as i32).max(0) as u32,
        inner.height,
    );
    let days = &weather.days[1..];
    let column_width = rest.width / days.len() as u32;
    for (i, day) in days.iter().enumerate() {
        let column = Area::new(
            rest.x + (column_width * i as u32) as i32,
            rest.y,
            column_width,
            rest.height,
        );
        // A faint divider, so the columns read as days rather than a row of
        // numbers.
        canvas.draw_vline(
            column.x,
            column.y + column.h(0.1) as i32,
            column.h(0.8) as u32,
            colors.foreground.lerp(colors.background, 0.85),
        );
        let size = column.h(0.16).min(column.width as f32 * 0.35);
        label(
            canvas,
            font,
            &day.label,
            column.centre_x(),
            column.y + column.h(0.12) as i32,
            size,
            colors.muted(),
            Align::Center,
            column.width,
        );
        icon(
            canvas,
            day.condition,
            true,
            (column.centre_x(), column.y + column.h(0.45) as i32),
            (column.h(0.36) as u32).min(column.width * 3 / 4),
            colors.background,
        );
        let high = Weather::degrees(day.high);
        let size = font.fitting_size(&high, column.width as f32 * 0.9, column.h(0.18));
        label(
            canvas,
            font,
            &high,
            column.centre_x(),
            column.y + column.h(0.75) as i32,
            size,
            colors.foreground,
            Align::Center,
            column.width,
        );
        let low = Weather::degrees(day.low);
        let size = font.fitting_size(&low, column.width as f32 * 0.9, column.h(0.13));
        label(
            canvas,
            font,
            &low,
            column.centre_x(),
            column.y + column.h(0.92) as i32,
            size,
            colors.muted(),
            Align::Center,
            column.width,
        );
    }
}

/// Cover, title, artist and progress. On a key the cover fills it; on a wide
/// tile it sits at the left with the text beside it.
fn media_card(
    canvas: &mut Canvas,
    area: Area,
    media: Option<&Media>,
    colors: Colors,
    font: Option<&Font>,
) {
    let Some(media) = media else {
        if let Some(font) = font {
            let text = "Nothing playing";
            let size = font.fitting_size(text, area.width as f32 * 0.85, area.h(0.14));
            label(
                canvas,
                font,
                text,
                area.centre_x(),
                area.centre_y(),
                size,
                colors.muted(),
                Align::Center,
                area.width,
            );
        }
        return;
    };
    let pad = (area.height / 12).max(4);
    let inner = area.inset(pad);
    let wide = inner.width as f32 >= inner.height as f32 * 1.4;

    if !wide {
        // A key: the cover is the key, with the progress along the bottom and
        // the state in the corner.
        if let Some(art) = media.art.as_ref().filter(|a| a.image.width() > 0) {
            blit_cover(canvas, &art.image, area);
        } else if let Some(font) = font {
            let title = media.title.as_deref().unwrap_or(&media.player);
            let size = font.fitting_size(title, inner.width as f32, inner.h(0.2));
            label(
                canvas,
                font,
                title,
                inner.centre_x(),
                inner.centre_y(),
                size,
                colors.foreground,
                Align::Center,
                inner.width,
            );
        }
        let thickness = (area.height / 24).max(3);
        progress(
            canvas,
            Area::new(
                area.x,
                area.bottom() - thickness as i32,
                area.width,
                thickness,
            ),
            media,
            colors,
            false,
        );
        if media.status != Status::Playing {
            state_glyph(
                canvas,
                media.status,
                (area.right() - (pad * 3) as i32, area.y + (pad * 3) as i32),
                pad * 3,
                colors.foreground,
                colors.background,
            );
        }
        return;
    }

    let mut text = inner;
    if let Some(art) = media.art.as_ref().filter(|a| a.image.width() > 0) {
        let side = inner.height;
        blit_cover(canvas, &art.image, Area::new(inner.x, inner.y, side, side));
        text.x += (side + pad * 2) as i32;
        text.width = text.width.saturating_sub(side + pad * 2);
    }
    let Some(font) = font else { return };

    let title = media.title.as_deref().unwrap_or(&media.player);
    let size = text.h(0.22).min(48.0);
    label(
        canvas,
        font,
        title,
        text.x,
        text.y + text.h(0.16) as i32,
        size,
        colors.foreground,
        Align::Left,
        text.width,
    );
    let byline = match (&media.artist, &media.album) {
        (Some(artist), _) => artist.clone(),
        (None, Some(album)) => album.clone(),
        (None, None) => media.player.clone(),
    };
    let size = text.h(0.14).min(32.0);
    label(
        canvas,
        font,
        &byline,
        text.x,
        text.y + text.h(0.4) as i32,
        size,
        colors.muted(),
        Align::Left,
        text.width,
    );

    // Glyph, elapsed, bar, total, along the bottom.
    let row_y = text.y + text.h(0.8) as i32;
    let glyph = (text.h(0.2) as u32).max(8);
    state_glyph(
        canvas,
        media.status,
        (text.x + glyph as i32 / 2, row_y),
        glyph,
        colors.foreground,
        colors.background,
    );
    let time_size = text.h(0.12).min(26.0);
    let elapsed = media.position.map(media::timestamp).unwrap_or_default();
    let total = media.length.map(media::timestamp).unwrap_or_default();
    let bar_left =
        text.x + glyph as i32 + pad as i32 + font.measure(&elapsed, time_size) as i32 + pad as i32;
    let bar_right = text.right() - font.measure(&total, time_size) as i32 - pad as i32;
    label(
        canvas,
        font,
        &elapsed,
        bar_left - pad as i32,
        row_y,
        time_size,
        colors.muted(),
        Align::Right,
        text.width,
    );
    label(
        canvas,
        font,
        &total,
        text.right(),
        row_y,
        time_size,
        colors.muted(),
        Align::Right,
        text.width,
    );
    if bar_right > bar_left {
        let thickness = (text.height / 18).max(4);
        progress(
            canvas,
            Area::new(
                bar_left,
                row_y - thickness as i32 / 2,
                (bar_right - bar_left) as u32,
                thickness,
            ),
            media,
            colors,
            true,
        );
    }
}

fn progress(canvas: &mut Canvas, track: Area, media: &Media, colors: Colors, rounded: bool) {
    let radius = if rounded { track.height / 2 } else { 0 };
    let Some(share) = media.progress() else {
        return;
    };
    blend_round_rect(canvas, track, radius, colors.foreground, 0.25);
    let filled = (track.width as f32 * share) as u32;
    if filled > 0 {
        fill_round_rect(
            canvas,
            Area {
                width: filled.max(radius * 2),
                ..track
            },
            radius,
            colors.accent,
        );
    }
}

/// Play, pause or stop, in a box `size` pixels across centred on `centre`.
fn state_glyph(
    canvas: &mut Canvas,
    status: Status,
    centre: (i32, i32),
    size: u32,
    color: Rgb,
    background: Rgb,
) {
    let half = size as i32 / 2;
    let (cx, cy) = centre;
    match status {
        // Showing what a press would do, as players do: pause while playing.
        Status::Playing => {
            let bar = (size / 3).max(2);
            canvas.fill_rect(cx - half + (size / 8) as i32, cy - half, bar, size, color);
            canvas.fill_rect(
                cx + half - (size / 8) as i32 - bar as i32,
                cy - half,
                bar,
                size,
                color,
            );
        }
        Status::Paused => {
            // A soft disc behind the triangle, so it reads over a cover.
            blend_disc(canvas, (cx, cy), half as u32 + 4, background, 0.6);
            fill_triangle(
                canvas,
                (cx - half / 2, cy - half),
                (cx - half / 2, cy + half),
                (cx + half, cy),
                color,
            );
        }
        Status::Stopped => {
            blend_disc(canvas, (cx, cy), half as u32 + 4, background, 0.6);
            canvas.fill_rect(
                cx - half * 3 / 4,
                cy - half * 3 / 4,
                (half * 3 / 2) as u32,
                (half * 3 / 2) as u32,
                color,
            );
        }
    }
}

/// What a key says about the state of what it stands for, besides its
/// look: a muted device, for the corner of a mute key, or, for a key with
/// states, how its state stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Speaker,
    Microphone,
    /// A "?": which state the key is in could not be read.
    Unknown,
    /// A "!": the key's command failed.
    Warning,
    /// Not in the corner but a short bar along the bottom edge: the key's
    /// command is still running.
    Pending,
}

/// A small struck-through speaker or microphone in the top-right corner of
/// `area`, saying the device is muted. For a key with states, a "?" or a
/// "!" there instead, knocked out of a disc or a triangle and struck through
/// by nothing, or for [`Glyph::Pending`] a bar along the bottom edge.
///
/// A glyph rather than a word, because the key's own label is already there
/// and the corner has room for about one character. `background` edges the
/// strike so it reads across the shape it crosses.
pub fn muted_glyph(canvas: &mut Canvas, area: Area, glyph: Glyph, color: Rgb, background: Rgb) {
    let size = (area.width.min(area.height) / 4).max(12);
    let margin = (size / 4) as i32;
    let (x0, y0) = (area.right() - margin - size as i32, area.y + margin);
    let s = size as f32;
    let at = |dx: f32, dy: f32| (x0 + (dx * s) as i32, y0 + (dy * s) as i32);
    let thickness = (size / 12).max(2);
    match glyph {
        Glyph::Speaker => {
            let (x, y) = at(0.06, 0.35);
            canvas.fill_rect(x, y, (s * 0.24) as u32, (s * 0.3) as u32, color);
            // The cone, widening away from the box.
            fill_triangle(canvas, at(0.28, 0.35), at(0.7, 0.06), at(0.7, 0.94), color);
            fill_triangle(canvas, at(0.28, 0.35), at(0.28, 0.65), at(0.7, 0.94), color);
        }
        Glyph::Microphone => {
            let (x, y) = at(0.34, 0.0);
            fill_round_rect(
                canvas,
                Area::new(x, y, (s * 0.32) as u32, (s * 0.56) as u32),
                (s * 0.16) as u32,
                color,
            );
            // The cup round the head, the stand and its foot.
            let cup = [
                (0.16, 0.34),
                (0.17, 0.48),
                (0.24, 0.62),
                (0.36, 0.71),
                (0.5, 0.74),
                (0.64, 0.71),
                (0.76, 0.62),
                (0.83, 0.48),
                (0.84, 0.34),
            ];
            for pair in cup.windows(2) {
                let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
                canvas.draw_line_thick(at(x1, y1), at(x2, y2), color, thickness);
            }
            canvas.draw_line_thick(at(0.5, 0.74), at(0.5, 0.94), color, thickness);
            canvas.draw_line_thick(at(0.3, 0.95), at(0.7, 0.95), color, thickness);
        }
        // Nothing here is muted, so nothing is struck through.
        Glyph::Unknown => {
            blend_disc(canvas, at(0.5, 0.5), size / 2, color, 1.0);
            // The hook, then the dot, in whatever the disc sits on.
            let hook = [
                (0.33, 0.37),
                (0.36, 0.27),
                (0.43, 0.2),
                (0.53, 0.19),
                (0.62, 0.23),
                (0.66, 0.31),
                (0.64, 0.39),
                (0.57, 0.45),
                (0.51, 0.51),
                (0.5, 0.6),
            ];
            for pair in hook.windows(2) {
                let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
                canvas.draw_line_thick(at(x1, y1), at(x2, y2), background, thickness);
            }
            blend_disc(canvas, at(0.5, 0.76), thickness, background, 1.0);
            return;
        }
        Glyph::Warning => {
            fill_triangle(canvas, at(0.5, 0.03), at(0.02, 0.93), at(0.98, 0.93), color);
            canvas.draw_line_thick(at(0.5, 0.35), at(0.5, 0.62), background, thickness);
            blend_disc(canvas, at(0.5, 0.78), thickness, background, 1.0);
            return;
        }
        Glyph::Pending => {
            pending_bar(canvas, area, color);
            return;
        }
    }
    // Edged in the background first, so the strike stays a line of its own
    // where it crosses the shape.
    let (from, to) = (at(0.0, 0.0), at(1.0, 1.0));
    canvas.draw_line_thick(from, to, background, thickness * 2 + 2);
    canvas.draw_line_thick(from, to, color, thickness);
}

/// A short bar centred along the bottom edge of `area`, saying something is
/// under way: still, because it is drawn once rather than per frame, and
/// short, so it does not read as a border.
fn pending_bar(canvas: &mut Canvas, area: Area, color: Rgb) {
    let height = (area.height / 24).max(3);
    let width = area.width * 2 / 5;
    let bar = Area::new(
        area.centre_x() - width as i32 / 2,
        area.bottom() - 3 - height as i32,
        width,
        height,
    );
    fill_round_rect(canvas, bar, height / 2, color);
}

/// A weather icon `size` pixels across, centred on `centre`.
fn icon(
    canvas: &mut Canvas,
    condition: Condition,
    is_day: bool,
    centre: (i32, i32),
    size: u32,
    background: Rgb,
) {
    let s = size as f32;
    let (cx, cy) = centre;
    let at = |dx: f32, dy: f32| (cx + (dx * s) as i32, cy + (dy * s) as i32);
    match condition {
        Condition::Clear if is_day => sun(canvas, centre, s * 0.24),
        Condition::Clear => moon(canvas, centre, s * 0.3, background),
        Condition::PartlyCloudy => {
            if is_day {
                sun(canvas, at(0.14, -0.14), s * 0.18);
            } else {
                moon(canvas, at(0.14, -0.14), s * 0.2, background);
            }
            cloud(canvas, at(-0.06, 0.1), s * 0.8, CLOUD);
        }
        Condition::Cloudy => cloud(canvas, centre, s, CLOUD),
        Condition::Fog => {
            let thickness = (size / 12).max(2);
            for (i, width) in [0.8, 0.6, 0.8].into_iter().enumerate() {
                let y = cy - (s * 0.25) as i32 + (i as f32 * s * 0.25) as i32;
                let w = (s * width) as u32;
                canvas.fill_rect(cx - w as i32 / 2, y, w, thickness, CLOUD);
            }
        }
        Condition::Drizzle | Condition::Rain | Condition::Snow | Condition::Storm => {
            let tint = if condition == Condition::Storm {
                STORM_CLOUD
            } else {
                CLOUD
            };
            cloud(canvas, at(0.0, -0.14), s * 0.9, tint);
            for i in 0..3 {
                let x = -0.22 + i as f32 * 0.22;
                match condition {
                    Condition::Rain => canvas.draw_line_thick(
                        at(x + 0.04, 0.2),
                        at(x - 0.04, 0.42),
                        RAIN,
                        (size / 18).max(2),
                    ),
                    Condition::Drizzle => canvas.fill_circle(at(x, 0.3), (size / 22).max(1), RAIN),
                    Condition::Snow => canvas.fill_circle(at(x, 0.3), (size / 16).max(2), SNOW),
                    _ => {}
                }
            }
            if condition == Condition::Storm {
                let t = (size / 14).max(2);
                canvas.draw_line_thick(at(0.04, 0.12), at(-0.08, 0.3), BOLT, t);
                canvas.draw_line_thick(at(-0.08, 0.3), at(0.06, 0.3), BOLT, t);
                canvas.draw_line_thick(at(0.06, 0.3), at(-0.06, 0.48), BOLT, t);
            }
        }
    }
}

fn sun(canvas: &mut Canvas, centre: (i32, i32), radius: f32) {
    canvas.fill_circle(centre, radius as u32, SUN);
    let thickness = (radius / 5.0).max(2.0) as u32;
    for ray in 0..8 {
        let angle = ray as f32 * std::f32::consts::FRAC_PI_4;
        let (sin, cos) = angle.sin_cos();
        let from = (
            centre.0 + (cos * radius * 1.4) as i32,
            centre.1 + (sin * radius * 1.4) as i32,
        );
        let to = (
            centre.0 + (cos * radius * 1.9) as i32,
            centre.1 + (sin * radius * 1.9) as i32,
        );
        canvas.draw_line_thick(from, to, SUN, thickness);
    }
}

/// A crescent: a disc with a disc of the background taken out of it.
fn moon(canvas: &mut Canvas, centre: (i32, i32), radius: f32, background: Rgb) {
    canvas.fill_circle(centre, radius as u32, MOON);
    canvas.fill_circle(
        (
            centre.0 + (radius * 0.45) as i32,
            centre.1 - (radius * 0.3) as i32,
        ),
        (radius * 0.85) as u32,
        background,
    );
}

/// Three puffs on a flat base, `width` pixels across.
fn cloud(canvas: &mut Canvas, centre: (i32, i32), width: f32, color: Rgb) {
    let (cx, cy) = centre;
    let r = width * 0.2;
    canvas.fill_circle(
        (cx - (width * 0.2) as i32, cy + (r * 0.3) as i32),
        r as u32,
        color,
    );
    canvas.fill_circle(
        (cx + (width * 0.02) as i32, cy - (r * 0.35) as i32),
        (r * 1.3) as u32,
        color,
    );
    canvas.fill_circle(
        (cx + (width * 0.22) as i32, cy + (r * 0.4) as i32),
        (r * 0.9) as u32,
        color,
    );
    canvas.fill_rect(
        cx - (width * 0.2) as i32,
        cy + (r * 0.3) as i32,
        (width * 0.42) as u32,
        (r * 1.0) as u32,
        color,
    );
}

/// Scale a cover to cover `area` (cropping, never letterboxing) and draw it.
fn blit_cover(canvas: &mut Canvas, image: &image::RgbaImage, area: Area) {
    cover_image(canvas, image, area, 1.0);
}

/// Scale an image to cover `area`, cropping the overflow evenly, and draw it
/// at `opacity`.
pub fn cover_image(canvas: &mut Canvas, image: &image::RgbaImage, area: Area, opacity: f32) {
    if area.width == 0 || area.height == 0 || image.width() == 0 {
        return;
    }
    let scale =
        (area.width as f32 / image.width() as f32).max(area.height as f32 / image.height() as f32);
    let width = ((image.width() as f32 * scale).ceil() as u32).max(area.width);
    let height = ((image.height() as f32 * scale).ceil() as u32).max(area.height);
    let scaled =
        image::imageops::resize(image, width, height, image::imageops::FilterType::Triangle);
    let offset_x = (width - area.width) / 2;
    let offset_y = (height - area.height) / 2;
    for y in 0..area.height {
        for x in 0..area.width {
            let [r, g, b, a] = scaled.get_pixel(x + offset_x, y + offset_y).0;
            if a > 0 {
                canvas.blend_pixel(
                    area.x + x as i32,
                    area.y + y as i32,
                    Rgb::new(r, g, b),
                    f32::from(a) / 255.0 * opacity,
                );
            }
        }
    }
}

/// A message over the bottom of the screen: what a knob just did, with a
/// bar when it set a level.
///
/// Drawn last, over whatever the screen shows, on a dark translucent strip
/// so it reads over a picture as well as over tiles.
pub fn osd(
    canvas: &mut Canvas,
    text: &str,
    level: Option<(f32, bool)>,
    colors: Colors,
    font: Option<&Font>,
) {
    let (width, height) = (canvas.width(), canvas.height());
    let strip = Area::new(24, height as i32 - 92, width.saturating_sub(48), 68);
    blend_round_rect(canvas, strip, 16, Rgb::new(8, 9, 12), 0.82);
    let inner = strip.inset(18);
    let text_width = if level.is_some() {
        inner.width * 45 / 100
    } else {
        inner.width
    };
    if let Some(font) = font {
        let size = font.fitting_size(text, text_width as f32, 30.0);
        label(
            canvas,
            font,
            text,
            inner.x,
            inner.centre_y(),
            size,
            colors.foreground,
            Align::Left,
            text_width,
        );
    }
    if let Some((fraction, muted)) = level {
        let track = Area::new(
            inner.x + text_width as i32 + 16,
            inner.centre_y() - 6,
            inner.width.saturating_sub(text_width + 16),
            12,
        );
        blend_round_rect(canvas, track, 6, colors.foreground, 0.2);
        let fill = if muted {
            Rgb::new(191, 97, 106)
        } else {
            colors.accent
        };
        let filled = (track.width as f32 * fraction.clamp(0.0, 1.0)) as u32;
        if filled > 0 {
            fill_round_rect(
                canvas,
                Area {
                    width: filled.max(12),
                    ..track
                },
                6,
                fill,
            );
        }
    }
}

/// Whether a pixel of `area` is inside its rounded corners.
fn inside_rounded(area: Area, radius: u32, x: i32, y: i32) -> bool {
    let r = radius.min(area.width / 2).min(area.height / 2) as i32;
    if r == 0 {
        return true;
    }
    // Distance into the corner square, if in one.
    let dx = (area.x + r - x).max(x - (area.right() - 1 - r)).max(0);
    let dy = (area.y + r - y).max(y - (area.bottom() - 1 - r)).max(0);
    dx * dx + dy * dy <= r * r
}

/// A rounded rectangle laid over what is there at `alpha`.
pub fn blend_round_rect(canvas: &mut Canvas, area: Area, radius: u32, color: Rgb, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    if alpha >= 1.0 {
        fill_round_rect(canvas, area, radius, color);
        return;
    }
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if inside_rounded(area, radius, x, y) {
                canvas.blend_pixel(x, y, color, alpha);
            }
        }
    }
}

/// Put back what was under a rounded rectangle's corners, from a copy taken
/// before it was drawn: a flat colour cannot, when what was there is a
/// picture.
pub fn restore_corners(canvas: &mut Canvas, area: Area, radius: u32, from: &Canvas) {
    let r = radius.min(area.width / 2).min(area.height / 2) as i32;
    for (x0, y0) in [
        (area.x, area.y),
        (area.right() - r, area.y),
        (area.x, area.bottom() - r),
        (area.right() - r, area.bottom() - r),
    ] {
        for y in y0..y0 + r {
            for x in x0..x0 + r {
                if !inside_rounded(area, radius, x, y) {
                    if let Some(pixel) = from.pixel(x, y) {
                        canvas.set_pixel(x, y, pixel);
                    }
                }
            }
        }
    }
}

/// A rectangle with rounded corners.
pub fn fill_round_rect(canvas: &mut Canvas, area: Area, radius: u32, color: Rgb) {
    let radius = radius.min(area.width / 2).min(area.height / 2);
    if radius == 0 {
        canvas.fill_rect(area.x, area.y, area.width, area.height, color);
        return;
    }
    let r = radius as i32;
    canvas.fill_rect(
        area.x + r,
        area.y,
        area.width - radius * 2,
        area.height,
        color,
    );
    canvas.fill_rect(
        area.x,
        area.y + r,
        area.width,
        area.height - radius * 2,
        color,
    );
    for (cx, cy) in [
        (area.x + r, area.y + r),
        (area.right() - r - 1, area.y + r),
        (area.x + r, area.bottom() - r - 1),
        (area.right() - r - 1, area.bottom() - r - 1),
    ] {
        canvas.fill_circle((cx, cy), radius, color);
    }
}

/// Paint back whatever a widget drew outside a rounded rectangle's corners.
///
/// A graph fills to the bottom edge of its area, which on a rounded card
/// leaves square corners poking out; this puts `outside` back over them.
pub fn clip_corners(canvas: &mut Canvas, area: Area, radius: u32, outside: Rgb) {
    let radius = radius.min(area.width / 2).min(area.height / 2) as i32;
    if radius == 0 {
        return;
    }
    let r2 = radius * radius;
    for dy in 0..radius {
        for dx in 0..radius {
            // Distance from the corner circle's centre to this pixel's centre.
            let (ox, oy) = (radius - dx, radius - dy);
            if ox * ox + oy * oy <= r2 {
                continue;
            }
            for (x, y) in [
                (area.x + dx, area.y + dy),
                (area.right() - 1 - dx, area.y + dy),
                (area.x + dx, area.bottom() - 1 - dy),
                (area.right() - 1 - dx, area.bottom() - 1 - dy),
            ] {
                canvas.set_pixel(x, y, outside);
            }
        }
    }
}

fn blend_disc(canvas: &mut Canvas, centre: (i32, i32), radius: u32, color: Rgb, alpha: f32) {
    let r = radius as i32;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                canvas.blend_pixel(centre.0 + dx, centre.1 + dy, color, alpha);
            }
        }
    }
}

fn fill_triangle(canvas: &mut Canvas, a: (i32, i32), b: (i32, i32), c: (i32, i32), color: Rgb) {
    let min_y = a.1.min(b.1).min(c.1);
    let max_y = a.1.max(b.1).max(c.1);
    let edges = [(a, b), (b, c), (c, a)];
    for y in min_y..=max_y {
        let crossings: Vec<f32> = edges
            .iter()
            .filter(|((_, y0), (_, y1))| (*y0 <= y && y < *y1) || (*y1 <= y && y < *y0))
            .map(|((x0, y0), (x1, y1))| {
                *x0 as f32 + (y - y0) as f32 * (x1 - x0) as f32 / (y1 - y0) as f32
            })
            .collect();
        if let [left, right] = crossings[..] {
            let (left, right) = (left.min(right) as i32, left.max(right) as i32);
            canvas.fill_rect(left, y, (right - left + 1) as u32, 1, color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn label(
    canvas: &mut Canvas,
    font: &Font,
    text: &str,
    x: i32,
    y: i32,
    size: f32,
    color: Rgb,
    align: Align,
    max_width: u32,
) {
    if text.is_empty() || size < 1.0 {
        return;
    }
    let style = TextStyle::new(font, size)
        .color(color)
        .align(align)
        .max_width(max_width.max(1));
    canvas.draw_text(text, x, y, &style);
}

#[cfg(test)]
mod tests {
    use super::super::weather::Day;
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    const BG: Rgb = Rgb::new(10, 10, 10);
    const FG: Rgb = Rgb::new(240, 240, 240);
    const ACCENT: Rgb = Rgb::new(255, 0, 0);

    fn colors() -> Colors {
        Colors {
            background: BG,
            foreground: FG,
            accent: ACCENT,
        }
    }

    fn state(widget: &Widget, values: &[f64]) -> SlotState {
        let mut state = SlotState::default();
        for value in values {
            state.update(
                Some(Reading::Value {
                    text: format!("{value}"),
                    value: Some(*value),
                }),
                widget,
            );
        }
        state
    }

    #[test]
    fn a_reading_can_be_drawn_as_nixie_tubes() {
        let cpu = Widget {
            view: Some(WidgetView::Nixie),
            ..Widget::of(WidgetKind::Cpu)
        };
        let mut state = SlotState::default();
        let reading = Reading::Value {
            text: "42%".to_string(),
            value: Some(42.0),
        };
        state.update(Some(reading), &cpu);
        let mut canvas = Canvas::filled(160, 160, BG);
        let area = Area::new(0, 0, 160, 160);
        widget(&mut canvas, area, &cpu, Some(&state), colors(), None, None);
        // Neon, warm and bright, where text would have been white.
        let neon = canvas
            .as_rgb()
            .chunks(3)
            .any(|p| p[0] > 200 && p[0] > p[1] && p[1] > p[2]);
        assert!(neon);
    }

    #[test]
    fn a_full_graph_reaches_the_bottom_right_in_the_accent() {
        let widget = Widget {
            view: Some(WidgetView::Graph),
            ..Widget::of(WidgetKind::Cpu)
        };
        let mut canvas = Canvas::filled(100, 100, BG);
        let full = state(&widget, &[100.0; 60]);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            &widget,
            Some(&full),
            colors(),
            None,
            None,
        );
        // The line runs along the top of the plot; under it is the fill.
        let under = canvas.pixel(50, 95).unwrap();
        let expected = BG.lerp(ACCENT, 0.35);
        // Blended rather than mixed, which rounds a step differently.
        assert!(
            under.r.abs_diff(expected.r) <= 1 && under.g.abs_diff(expected.g) <= 1,
            "{under:?}"
        );
    }

    #[test]
    fn an_empty_graph_leaves_the_plot_bare() {
        let widget = Widget {
            view: Some(WidgetView::Graph),
            ..Widget::of(WidgetKind::Cpu)
        };
        let mut canvas = Canvas::filled(100, 100, BG);
        let idle = state(&widget, &[0.0; 60]);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            &widget,
            Some(&idle),
            colors(),
            None,
            None,
        );
        assert_eq!(canvas.pixel(50, 60), Some(BG));
    }

    #[test]
    fn a_bar_fills_in_proportion() {
        let widget = Widget {
            view: Some(WidgetView::Bar),
            ..Widget::of(WidgetKind::Memory)
        };
        let mut canvas = Canvas::filled(100, 100, BG);
        let half = state(&widget, &[50.0]);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            &widget,
            Some(&half),
            colors(),
            None,
            None,
        );
        let row = 100 - 10 - 6;
        assert_eq!(canvas.pixel(20, row), Some(ACCENT));
        assert_ne!(canvas.pixel(80, row), Some(ACCENT));
    }

    #[test]
    fn a_graph_in_a_tile_stays_inside_it() {
        let widget = Widget {
            view: Some(WidgetView::Graph),
            ..Widget::of(WidgetKind::Cpu)
        };
        let mut canvas = Canvas::filled(300, 100, BG);
        let full = state(&widget, &[100.0; 60]);
        super::widget(
            &mut canvas,
            Area::new(100, 0, 100, 100),
            &widget,
            Some(&full),
            colors(),
            None,
            None,
        );
        for y in 0..100 {
            assert_eq!(canvas.pixel(99, y), Some(BG));
            assert_eq!(canvas.pixel(200, y), Some(BG));
        }
    }

    #[test]
    fn weather_and_media_draw_without_a_font() {
        // No font on a CI runner must not panic, and the icon still shows.
        let weather = Weather {
            temperature: 21.0,
            condition: Condition::Clear,
            is_day: true,
            units: galdeck_model::Units::Celsius,
            days: vec![
                Day {
                    label: "Thu".into(),
                    high: 24.0,
                    low: 12.0,
                    condition: Condition::Clear,
                },
                Day {
                    label: "Fri".into(),
                    high: 20.0,
                    low: 11.0,
                    condition: Condition::Rain,
                },
            ],
        };
        let widget = Widget::of(WidgetKind::Weather);
        let mut s = SlotState::default();
        s.update(Some(Reading::Weather(weather)), &widget);
        let mut canvas = Canvas::filled(360, 128, BG);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 360, 128),
            &widget,
            Some(&s),
            colors(),
            None,
            None,
        );

        let media = Media {
            player: "spotify".into(),
            status: Status::Paused,
            title: Some("Dreamy Stupor".into()),
            artist: Some("Harris".into()),
            album: None,
            position: Some(Duration::from_secs(31)),
            length: Some(Duration::from_secs(120)),
            art: Some(media::Art {
                url: "x".into(),
                image: Arc::new(image::RgbaImage::from_pixel(
                    8,
                    8,
                    image::Rgba([0, 255, 0, 255]),
                )),
            }),
        };
        let widget = Widget::of(WidgetKind::Media);
        let mut s = SlotState::default();
        s.update(Some(Reading::Media(Some(media))), &widget);
        let mut key = Canvas::filled(160, 160, BG);
        super::widget(
            &mut key,
            Area::new(0, 0, 160, 160),
            &widget,
            Some(&s),
            colors(),
            None,
            None,
        );
        // The cover fills the key.
        assert_eq!(key.pixel(40, 80), Some(Rgb::new(0, 255, 0)));
    }

    #[test]
    fn a_gauge_fills_its_arc_in_proportion() {
        let widget = Widget {
            view: Some(WidgetView::Gauge),
            ..Widget::of(WidgetKind::Memory)
        };
        let half = state(&widget, &[50.0]);
        let mut canvas = Canvas::filled(100, 100, BG);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            &widget,
            Some(&half),
            colors(),
            None,
            None,
        );
        // Half of a 270-degree arc from bottom-left ends straight up: the
        // left of the ring is filled, the right is not.
        let lit = |x, y| canvas.pixel(x, y).is_some_and(|p| p.r > 150 && p.g < 80);
        assert!(
            (5..30).any(|x| lit(x, 52)),
            "left of the ring should be filled"
        );
        assert!(
            !(70..97).any(|x| lit(x, 52)),
            "right of the ring should not be"
        );
    }

    /// Draw `widget` on a 100-pixel key with these readings.
    fn drawn(widget: &Widget, values: &[f64]) -> Canvas {
        let mut canvas = Canvas::filled(100, 100, BG);
        let readings = state(widget, values);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            widget,
            Some(&readings),
            colors(),
            None,
            None,
        );
        canvas
    }

    /// How many separate stretches of the accent a row crosses.
    fn runs(canvas: &Canvas, y: i32) -> usize {
        let lit: Vec<bool> = (0..100)
            .map(|x| canvas.pixel(x, y).is_some_and(|p| p.r > 150 && p.g < 80))
            .collect();
        lit.windows(2).filter(|pair| !pair[0] && pair[1]).count() + usize::from(lit[0])
    }

    #[test]
    fn a_line_graph_leaves_what_is_under_the_line_alone() {
        // Said by its look, as a theme would.
        let widget = Widget {
            view: Some(WidgetView::Graph),
            look: galdeck_model::WidgetLook {
                graph: Some(GraphStyle::Line),
                ..Default::default()
            },
            ..Widget::of(WidgetKind::Cpu)
        };
        let canvas = drawn(&widget, &[100.0; 60]);
        assert_eq!(canvas.pixel(50, 95), Some(BG));
    }

    #[test]
    fn a_column_graph_draws_columns_with_gaps() {
        let widget = Widget {
            view: Some(WidgetView::Graph),
            graph: Some(GraphStyle::Bars),
            ..Widget::of(WidgetKind::Cpu)
        };
        let canvas = drawn(&widget, &[100.0; 60]);
        assert!(runs(&canvas, 95) > 10, "{} columns", runs(&canvas, 95));
    }

    #[test]
    fn a_segmented_bar_lights_whole_steps() {
        let widget = Widget {
            view: Some(WidgetView::Bar),
            bar: Some(BarStyle::Segmented),
            segments: Some(10),
            ..Widget::of(WidgetKind::Memory)
        };
        let canvas = drawn(&widget, &[50.0]);
        assert_eq!(runs(&canvas, 100 - 10 - 6), 5);
    }

    #[test]
    fn a_flat_bar_has_square_ends() {
        let flat = Widget {
            view: Some(WidgetView::Bar),
            bar: Some(BarStyle::Flat),
            ..Widget::of(WidgetKind::Memory)
        };
        let rounded = Widget {
            bar: None,
            ..flat.clone()
        };
        // The track's top-left corner: 10 in and 22 up from the bottom.
        assert_eq!(drawn(&flat, &[100.0]).pixel(10, 78), Some(ACCENT));
        assert_ne!(drawn(&rounded, &[100.0]).pixel(10, 78), Some(ACCENT));
    }

    #[test]
    fn a_ring_that_goes_all_the_way_round_is_lit_at_the_bottom() {
        let lit = |canvas: &Canvas, y| canvas.pixel(50, y).is_some_and(|p| p.r > 150 && p.g < 80);
        let ring = Widget {
            view: Some(WidgetView::Gauge),
            sweep: Some(360),
            ..Widget::of(WidgetKind::Memory)
        };
        assert!(lit(&drawn(&ring, &[100.0]), 89));
        // The usual dial is open there.
        let dial = Widget {
            sweep: None,
            ..ring.clone()
        };
        assert!(!lit(&drawn(&dial, &[100.0]), 92));
    }

    #[test]
    fn a_clock_face_points_its_hands_at_the_time() {
        let widget = Widget {
            view: Some(WidgetView::Analog),
            color: None,
            ..Widget::of(WidgetKind::Clock)
        };
        let mut at_three = SlotState::default();
        // 03:00:00 -- the hour hand points right, the second hand up.
        at_three.update(
            Some(Reading::Value {
                text: "03:00".into(),
                value: Some(3.0 * 3600.0),
            }),
            &widget,
        );
        let mut canvas = Canvas::filled(100, 100, BG);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            &widget,
            Some(&at_three),
            colors(),
            None,
            None,
        );
        assert_eq!(canvas.pixel(68, 50), Some(FG), "hour hand towards three");
        assert_eq!(
            canvas.pixel(50, 20),
            Some(ACCENT),
            "second hand towards twelve"
        );
        assert_ne!(canvas.pixel(32, 50), Some(FG), "nothing towards nine");
    }

    #[test]
    fn clipped_corners_are_the_outside_colour_again() {
        let mut canvas = Canvas::filled(40, 40, ACCENT);
        clip_corners(&mut canvas, Area::new(0, 0, 40, 40), 10, BG);
        for (x, y) in [(0, 0), (39, 0), (0, 39), (39, 39)] {
            assert_eq!(canvas.pixel(x, y), Some(BG));
        }
        assert_eq!(canvas.pixel(20, 0), Some(ACCENT));
        assert_eq!(canvas.pixel(20, 20), Some(ACCENT));
    }

    #[test]
    fn restored_corners_show_what_was_under_them() {
        let before = Canvas::filled(40, 40, BG);
        let mut canvas = Canvas::filled(40, 40, ACCENT);
        restore_corners(&mut canvas, Area::new(0, 0, 40, 40), 10, &before);
        assert_eq!(canvas.pixel(0, 0), Some(BG));
        assert_eq!(canvas.pixel(39, 39), Some(BG));
        assert_eq!(canvas.pixel(20, 20), Some(ACCENT));
    }

    #[test]
    fn a_translucent_card_tints_what_is_under_it() {
        let mut canvas = Canvas::filled(40, 40, Rgb::new(0, 0, 0));
        blend_round_rect(
            &mut canvas,
            Area::new(0, 0, 40, 40),
            8,
            Rgb::new(200, 200, 200),
            0.5,
        );
        assert_eq!(canvas.pixel(0, 0), Some(Rgb::new(0, 0, 0)));
        let middle = canvas.pixel(20, 20).unwrap();
        assert!((95..=105).contains(&middle.r), "{middle:?}");
    }

    #[test]
    fn the_osd_sits_at_the_bottom_with_its_bar() {
        let mut canvas = Canvas::filled(720, 384, Rgb::new(200, 200, 200));
        osd(
            &mut canvas,
            "Volume 50%",
            Some((0.5, false)),
            colors(),
            None,
        );
        // The top of the screen is untouched; the strip darkens the bottom.
        assert_eq!(canvas.pixel(360, 100), Some(Rgb::new(200, 200, 200)));
        assert!(canvas.pixel(40, 350).unwrap().r < 100);
        // Half the bar is filled in the accent, the far end is not.
        let bar_y = 384 - 92 + 34;
        assert_eq!(canvas.pixel(360, bar_y), Some(ACCENT));
        assert_ne!(canvas.pixel(660, bar_y), Some(ACCENT));
    }

    #[test]
    fn round_rects_stay_in_bounds() {
        let mut canvas = Canvas::filled(20, 20, BG);
        fill_round_rect(&mut canvas, Area::new(5, 5, 10, 4), 2, ACCENT);
        assert_eq!(canvas.pixel(4, 6), Some(BG));
        assert_eq!(canvas.pixel(15, 6), Some(BG));
        assert_eq!(canvas.pixel(10, 6), Some(ACCENT));
    }

    #[test]
    fn a_timer_bar_holds_what_is_left_of_its_length() {
        let widget = Widget {
            view: Some(WidgetView::Bar),
            duration: Some("10m".into()),
            ..Widget::of(WidgetKind::Timer)
        };
        let mut canvas = Canvas::filled(100, 100, BG);
        // Five minutes of ten left.
        let half = state(&widget, &[300.0]);
        super::widget(
            &mut canvas,
            Area::new(0, 0, 100, 100),
            &widget,
            Some(&half),
            colors(),
            None,
            None,
        );
        let row = 100 - 10 - 6;
        assert_eq!(canvas.pixel(20, row), Some(ACCENT));
        assert_ne!(canvas.pixel(80, row), Some(ACCENT));
    }

    #[test]
    fn a_muted_glyph_keeps_to_the_top_right_corner() {
        for glyph in [
            Glyph::Speaker,
            Glyph::Microphone,
            Glyph::Unknown,
            Glyph::Warning,
        ] {
            let mut canvas = Canvas::filled(160, 160, BG);
            muted_glyph(&mut canvas, Area::new(0, 0, 160, 160), glyph, FG, BG);
            let drawn: Vec<(u32, u32)> = (0..160)
                .flat_map(|y| (0..160).map(move |x| (x, y)))
                .filter(|&(x, y)| canvas.pixel(x as i32, y as i32) != Some(BG))
                .collect();
            assert!(!drawn.is_empty(), "{glyph:?} drew nothing");
            assert!(
                drawn.iter().all(|&(x, y)| x >= 100 && y < 60),
                "{glyph:?} strayed out of its corner"
            );
        }
    }

    #[test]
    fn a_state_s_glyphs_are_knocked_out_and_a_pending_bar_keeps_to_the_bottom() {
        let key = Area::new(0, 0, 160, 160);
        // The "?" is drawn in the background's colour inside its disc, so
        // the middle of the disc is not all one colour.
        let mut canvas = Canvas::filled(160, 160, BG);
        muted_glyph(&mut canvas, key, Glyph::Unknown, FG, BG);
        let disc: Vec<Option<Rgb>> = (12..48)
            .flat_map(|y| (112..148).map(move |x| (x, y)))
            .map(|(x, y)| canvas.pixel(x, y))
            .collect();
        assert!(disc.contains(&Some(FG)) && disc.contains(&Some(BG)));

        let mut canvas = Canvas::filled(160, 160, BG);
        muted_glyph(&mut canvas, key, Glyph::Pending, FG, BG);
        let drawn: Vec<(u32, u32)> = (0..160)
            .flat_map(|y| (0..160).map(move |x| (x, y)))
            .filter(|&(x, y)| canvas.pixel(x as i32, y as i32) != Some(BG))
            .collect();
        assert!(!drawn.is_empty(), "the bar drew nothing");
        let in_place = |&(x, y): &(u32, u32)| y >= 140 && (40..120).contains(&x);
        assert!(
            drawn.iter().all(in_place),
            "the bar strayed from the bottom's middle"
        );
    }
}
