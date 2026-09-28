//! Themes, described and drawn for the editor.
//!
//! The editor is a thin client: it shows what the daemon says a theme comes
//! to, and asks the daemon to draw it. So where each value comes from -- the
//! theme itself, a theme it extends, or the built-in look -- is worked out
//! here, from the same model the deck is drawn from, rather than guessed at
//! in JavaScript from the text of a file.

use std::path::Path;

use galdeck::{Canvas, Font, Rgb};
use galdeck_ipc::{
    BackdropInfo, Diagnostic, LightingInfo, LightingKeyInfo, MotionAnimationInfo, MotionInfo,
    PaletteEntryInfo, PressInfo, ReactiveInfo, StyleFieldInfo, ThemeInfo, ThemePreview,
    WidgetKindLookInfo, WidgetLookInfo, WidgetLooksInfo,
};
use galdeck_model::motion::PRESS_STRENGTH;
use galdeck_model::theme::{resolve, ResolvedValue, StyleSource, StyleValue, STYLE_FIELDS};
use galdeck_model::{
    Animation, ColorRef, ConfigDocument, Diagnostics, Lighting, MotionStyle, Patch, PressKind,
    ResolvedPalette, ResolvedStyle, Theme, Value, WidgetLook, WidgetLooks, Workspace,
};

use crate::backdrop::{Geometry, Scene};
use crate::engine::{hex, CRITICAL};
use crate::render;
use crate::widgets::{demo, draw};

/// Longest a theme's id may be. It names a file, and a line in a list.
const MAX_ID_LEN: usize = 40;

/// Which frame of a moving background a preview shows: a few seconds in,
/// once an animation has had time to get going, rather than its first.
const PREVIEW_TICK: u64 = 30;

/// Every theme in the workspace, by id.
pub fn describe(workspace: &Workspace) -> Vec<ThemeInfo> {
    workspace
        .themes
        .keys()
        .map(|id| describe_one(workspace, id))
        .collect()
}

fn describe_one(workspace: &Workspace, id: &str) -> ThemeInfo {
    let chain = workspace.theme_chain(id);
    let (palette, style) = resolved(workspace, id);

    // Nearest first, so the first theme to set a field is where it comes from.
    let layers: Vec<Layer<'_>> = chain
        .iter()
        .map(|(id, theme)| (*id, theme.style.fields()))
        .collect();
    // The same without this theme's own settings, in this theme's palette:
    // what emptying a field would leave.
    let ancestors: Vec<(StyleSource, &galdeck_model::StyleLayer)> = chain
        .iter()
        .skip(1)
        .rev()
        .map(|(_, theme)| (StyleSource::Theme, &theme.style))
        .collect();
    let (below, _) = resolve(&ancestors, &palette, "style", &mut Diagnostics::new());
    let style_fields = style
        .fields()
        .into_iter()
        .zip(below.fields())
        .enumerate()
        .map(|(index, ((field, value), (_, beneath)))| StyleFieldInfo {
            field: field.to_string(),
            kind: kind_of(value).to_string(),
            value: layers
                .first()
                .and_then(|(_, fields)| written_style(fields[index].1)),
            resolved: resolved_text(value),
            origin: set_by(&layers, index),
            inherited: resolved_text(beneath),
            inherited_origin: set_by(layers.get(1..).unwrap_or_default(), index),
        })
        .collect();

    let own = chain.first().map(|(_, theme)| *theme);
    ThemeInfo {
        id: id.to_string(),
        file: format!("themes/{id}.toml"),
        name: own.and_then(|theme| theme.name.clone()),
        extends: own.and_then(|theme| theme.extends.clone()),
        ancestors: chain.iter().skip(1).map(|(id, _)| id.to_string()).collect(),
        profiles: workspace
            .profiles
            .iter()
            .filter(|(_, profile)| profile.theme.as_deref() == Some(id))
            .map(|(profile, _)| profile.clone())
            .collect(),
        extended_by: workspace
            .themes
            .iter()
            .filter(|(_, theme)| theme.extends.as_deref() == Some(id))
            .map(|(theme, _)| theme.clone())
            .collect(),
        palette: palette_entries(&chain, &palette),
        style: style_fields,
        background: background_info(&chain, &palette, style.lcd_bg),
        lighting: own
            .and_then(|theme| theme.lighting.as_ref())
            .map(|layer| lighting_info(layer, &palette)),
        inherited_lighting: fold_lighting(chain.get(1..).unwrap_or_default())
            .map(|layer| lighting_info(&layer, &palette)),
        widgets: own
            .and_then(|theme| theme.widgets.as_ref())
            .map(|looks| looks_info(looks, &palette)),
        inherited_widgets: fold_looks(chain.get(1..).unwrap_or_default())
            .map(|looks| looks_info(&looks, &palette)),
        motion: own
            .and_then(|theme| theme.motion.as_ref())
            .map(|motion| motion_info(motion, &palette)),
        inherited_motion: fold_motion(chain.get(1..).unwrap_or_default())
            .map(|motion| motion_info(&motion, &palette)),
    }
}

/// Start a theme in `themes/<id>.toml`, and say which file it is.
///
/// One that extends another is two lines; a copy is the other theme's file,
/// comments and all, under a new name. Nothing already there is replaced.
pub fn create(
    dir: &Path,
    workspace: &Workspace,
    id: &str,
    name: Option<&str>,
    extends: Option<&str>,
    copy: Option<&str>,
) -> Result<String, String> {
    if !is_valid_id(id) {
        return Err(format!(
            "a theme's id is its file's name: lowercase letters, digits, - and _, \
             starting with a letter or a digit, at most {MAX_ID_LEN} long"
        ));
    }
    let file = format!("themes/{id}.toml");
    let path = dir.join(&file);
    if workspace.themes.contains_key(id) || path.exists() {
        return Err(format!("there is already a theme {id:?}"));
    }
    let known = |other: &str| {
        if workspace.themes.contains_key(other) {
            Ok(())
        } else {
            Err(format!("there is no theme {other:?}"))
        }
    };
    let text = match (extends, copy) {
        (Some(_), Some(_)) => {
            return Err("a new theme can extend a theme or copy one, not both".into())
        }
        (None, Some(source)) => {
            known(source)?;
            let from = format!("themes/{source}.toml");
            std::fs::read_to_string(dir.join(&from)).map_err(|e| format!("reading {from}: {e}"))?
        }
        (Some(parent), None) => {
            known(parent)?;
            String::new()
        }
        (None, None) => String::new(),
    };

    let mut patches = Vec::new();
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        patches.push(Patch::Set {
            path: "name".into(),
            value: Value::String(name.into()),
        });
    }
    if let Some(parent) = extends {
        patches.push(Patch::Set {
            path: "extends".into(),
            value: Value::String(parent.into()),
        });
    }
    let mut document = ConfigDocument::parse(&path, &text).map_err(|d| d.render())?;
    let staged = document.preview(&patches, None).map_err(|d| d.render())?;
    document.commit(staged).map_err(|d| d.render())?;
    document
        .save()
        .map_err(|e| format!("writing {file}: {e}"))?;
    Ok(file)
}

/// Draw a theme: a few keys and the screen, over its background if it has
/// one.
///
/// `workspace` already has whatever unsaved edits are being previewed in
/// it; `diagnostics` are what is wrong with them, passed through.
pub fn preview(
    workspace: &Workspace,
    id: &str,
    geometry: Geometry,
    font: Option<&Font>,
    diagnostics: Vec<Diagnostic>,
) -> ThemePreview {
    let chain = workspace.theme_chain(id);
    let (palette, style) = resolved(workspace, id);

    let scene = chain
        .iter()
        .find_map(|(_, theme)| theme.background.as_ref())
        .filter(|backdrop| !backdrop.is_empty())
        .and_then(|backdrop| {
            let colors = crate::backdrop::colors_for(backdrop, &palette, style.lcd_bg);
            Scene::load(backdrop, colors, geometry.clone())
                .map_err(|problem| log::debug!("previewing {id} without its background: {problem}"))
                .ok()
        });
    let frame = scene.as_ref().map(|scene| scene.render(PREVIEW_TICK));
    let backed = scene.as_ref().zip(frame.as_ref());

    // Each key at the size the deck draws it, over its own slice of the
    // background, so a picture reads across them the way it will there.
    let key = |index: u8| {
        let size = geometry
            .keys
            .get(usize::from(index))
            .map(|rect| (rect.width, rect.height))
            .filter(|&(width, height)| width > 0 && height > 0)
            .unwrap_or_else(galdeck::Button::size);
        backed
            .and_then(|(scene, frame)| scene.key(frame, index, size))
            .unwrap_or_else(|| Canvas::filled(size.0, size.1, style.key_bg))
    };
    // A widget with made-up readings, in this theme's look for its kind,
    // drawn as the deck would draw it: in the theme's view for that kind if
    // it has one, else in `view`. `alarm` is a reading past its critical
    // threshold.
    let sample = |mut canvas: Canvas, kind: &str, view: Option<&str>, alarm: Option<Rgb>| {
        let Ok(mut widget) = toml::from_str::<galdeck_model::Widget>(&format!("kind = \"{kind}\""))
        else {
            return canvas;
        };
        widget.look = workspace.theme_widget_look(id, widget.kind);
        if widget.view() == galdeck_model::WidgetView::Text {
            widget.view = view.and_then(|view| toml::Value::String(view.into()).try_into().ok());
        }
        let resolve = |color: Option<&ColorRef>| {
            color.and_then(|color| palette.resolve(color, "", &mut Diagnostics::new()))
        };
        let area = draw::Area::new(0, 0, canvas.width(), canvas.height());
        let mut background = style.key_bg;
        if let Some(color) = resolve(widget.background()) {
            draw::blend_round_rect(&mut canvas, area, 0, color, widget.opacity());
            background = color;
        }
        let colors = draw::Colors {
            background,
            foreground: style.key_label_color,
            accent: alarm
                .or_else(|| resolve(widget.color()))
                .unwrap_or(style.key_label_color),
        };
        let state = demo::state(&widget);
        draw::widget(&mut canvas, area, &widget, Some(&state), colors, font, None);
        canvas
    };
    let critical = palette.get("critical").unwrap_or(CRITICAL);
    let mut keys = vec![
        render::key_over(key(0), &style, None, Some("Mute"), font),
        sample(key(1), "cpu", Some("gauge"), None),
        sample(key(2), "temperature", Some("graph"), Some(critical)),
        sample(key(3), "memory", Some("bar"), None),
        sample(key(4), "clock", None, None),
    ];
    // A key mid-press, when the theme answers presses: a picture cannot
    // move, so the answer is shown where it is strongest, as the deck draws
    // it -- over the key, under its label.
    let motion = fold_motion(&chain);
    let press = motion
        .as_ref()
        .and_then(|motion| motion.press.as_ref())
        .filter(|press| press.kind != PressKind::None);
    if let Some(press) = press {
        let color = match press.kind {
            PressKind::Dim => Rgb::BLACK,
            _ => press
                .color
                .as_ref()
                .and_then(|color| palette.resolve(color, "", &mut Diagnostics::new()))
                .or_else(|| palette.get("accent"))
                .unwrap_or(Rgb::WHITE),
        };
        let mut canvas = key(5);
        let area = draw::Area::new(0, 0, canvas.width(), canvas.height());
        draw::blend_round_rect(&mut canvas, area, 0, color, PRESS_STRENGTH);
        keys.push(render::key_over(
            canvas,
            &style,
            None,
            Some("Pressed"),
            font,
        ));
    }

    let (width, height) = galdeck::Lcd::size();
    let size = (u32::from(width), u32::from(height));
    let screen = backed
        .and_then(|(scene, frame)| scene.lcd(frame, size))
        .unwrap_or_else(|| Canvas::filled(size.0, size.1, style.lcd_bg));
    let title = chain
        .first()
        .and_then(|(_, theme)| theme.name.clone())
        .unwrap_or_else(|| id.to_string());
    let screen = render::lcd_over(screen, &style, &title, font);

    ThemePreview {
        keys: keys.iter().filter_map(data_url).collect(),
        lcd: data_url(&screen).unwrap_or_default(),
        ring: hex(style.ring),
        palette: palette_entries(&chain, &palette),
        lighting: fold_lighting(&chain).map(|layer| lighting_info(&layer, &palette)),
        motion: motion.map(|motion| motion_info(&motion, &palette)),
        diagnostics,
    }
}

/// One theme's style, by field, with the theme's id.
type Layer<'a> = (&'a str, [(&'static str, StyleValue<'a>); STYLE_FIELDS]);

/// Which of these themes, nearest first, sets field `index`: the first that
/// does, or `builtin` when none of them does.
fn set_by(layers: &[Layer<'_>], index: usize) -> String {
    layers
        .iter()
        .find(|(_, fields)| fields[index].1.is_set())
        .map_or("builtin", |(id, _)| id)
        .to_string()
}

/// A theme's palette and style, its chain folded in and every default
/// applied.
fn resolved(workspace: &Workspace, id: &str) -> (ResolvedPalette, ResolvedStyle) {
    // Problems are the validator's to report, against the file they are in;
    // here they would only be reported a second time, in the wrong place.
    let mut quiet = Diagnostics::new();
    let (layer, palette) = workspace.theme_for(Some(id), &mut quiet);
    let (style, _) = resolve(
        &[(StyleSource::Theme, &layer)],
        &palette,
        "style",
        &mut quiet,
    );
    (palette, style)
}

/// Every colour a theme can name: its own, then each one it inherits and
/// does not define again.
fn palette_entries(chain: &[(&str, &Theme)], palette: &ResolvedPalette) -> Vec<PaletteEntryInfo> {
    let mut entries: Vec<PaletteEntryInfo> = Vec::new();
    for (origin, theme) in chain {
        for (name, value) in &theme.palette.0 {
            if entries.iter().any(|entry| entry.name == *name) {
                continue;
            }
            entries.push(PaletteEntryInfo {
                name: name.clone(),
                value: written(value),
                hex: palette.get(name).map(hex),
                origin: origin.to_string(),
            });
        }
    }
    entries
}

/// The nearest background in the chain, as the page editor describes one.
fn background_info(
    chain: &[(&str, &Theme)],
    palette: &ResolvedPalette,
    lcd_bg: Rgb,
) -> Option<BackdropInfo> {
    let (origin, backdrop) = chain
        .iter()
        .find_map(|(id, theme)| Some((*id, theme.background.as_ref()?)))?;
    Some(BackdropInfo {
        origin: "theme".into(),
        theme: Some(origin.to_string()),
        span: snake(&format!("{:?}", backdrop.span)),
        image: backdrop
            .image
            .as_ref()
            .map(|path| path.display().to_string()),
        animation: backdrop.animation.map(|motion| motion.name().to_string()),
        colors: backdrop.colors.iter().map(written).collect(),
        colors_hex: crate::backdrop::colors_for(backdrop, palette, lcd_bg)
            .into_iter()
            .map(hex)
            .collect(),
        fps: backdrop.fps(),
        speed: backdrop.speed(),
        dim: backdrop.dim(),
    })
}

/// These themes' `[lighting]`, folded so the first wins, or `None` when
/// none of them has any.
fn fold_lighting(chain: &[(&str, &Theme)]) -> Option<Lighting> {
    let mut folded: Option<Lighting> = None;
    for (_, theme) in chain.iter().rev() {
        if let Some(layer) = &theme.lighting {
            folded.get_or_insert_with(Lighting::default).overlay(layer);
        }
    }
    folded
}

fn lighting_info(layer: &Lighting, palette: &ResolvedPalette) -> LightingInfo {
    LightingInfo {
        effect: layer.effect.map(|effect| snake(&format!("{effect:?}"))),
        colors: layer
            .colors
            .as_ref()
            .map(|colors| colors.iter().map(written).collect()),
        colors_hex: layer
            .colors
            .iter()
            .flatten()
            .map(|color| resolved_hex(palette, color))
            .collect(),
        speed: layer.speed,
        brightness: layer.brightness,
        bar: layer.bar.as_ref().map(written),
        bar_hex: layer
            .bar
            .as_ref()
            .and_then(|color| resolved_hex(palette, color)),
        keys: layer
            .keys
            .iter()
            .map(|(keys, color)| LightingKeyInfo {
                keys: keys.clone(),
                color: written(color),
                hex: resolved_hex(palette, color),
            })
            .collect(),
        reactive: layer.reactive.as_ref().map(|reactive| ReactiveInfo {
            effect: reactive.effect.map(|effect| snake(&format!("{effect:?}"))),
            color: reactive.color.as_ref().map(written),
            color_hex: reactive
                .color
                .as_ref()
                .and_then(|color| resolved_hex(palette, color)),
            fade_ms: reactive.fade_ms,
        }),
    }
}

/// These themes' `[motion]`, folded so the first wins a setting at a time,
/// or `None` when none of them has any.
fn fold_motion(chain: &[(&str, &Theme)]) -> Option<MotionStyle> {
    chain
        .iter()
        .filter_map(|(_, theme)| theme.motion.as_ref())
        .fold(None, |folded: Option<MotionStyle>, layer| {
            Some(folded.unwrap_or_default().or(layer))
        })
}

fn motion_info(motion: &MotionStyle, palette: &ResolvedPalette) -> MotionInfo {
    let animation = |animation: &Animation| MotionAnimationInfo {
        kind: snake(&format!("{:?}", animation.kind)),
        period_ms: animation.period_ms,
        to: animation.to.as_ref().map(written),
        to_hex: animation
            .to
            .as_ref()
            .and_then(|color| resolved_hex(palette, color)),
    };
    MotionInfo {
        press: motion.press.as_ref().map(|press| PressInfo {
            kind: press.kind.name().to_string(),
            color: press.color.as_ref().map(written),
            color_hex: press
                .color
                .as_ref()
                .and_then(|color| resolved_hex(palette, color)),
            ms: press.ms,
        }),
        alarm: motion.alarm.as_ref().map(animation),
        rings: motion.rings.as_ref().map(animation),
    }
}

/// These themes' `[widgets]`, folded so the first wins -- each kind's look
/// over the same kind's further along, and every widget's likewise -- or
/// `None` when none of them has any.
fn fold_looks(chain: &[(&str, &Theme)]) -> Option<WidgetLooks> {
    let mut folded: Option<WidgetLooks> = None;
    for looks in chain.iter().filter_map(|(_, theme)| theme.widgets.as_ref()) {
        let into = folded.get_or_insert_with(WidgetLooks::default);
        into.all = std::mem::take(&mut into.all).or(&looks.all);
        for (kind, look) in &looks.kinds {
            match into.kinds.iter_mut().find(|(k, _)| k == kind) {
                Some((_, nearer)) => *nearer = std::mem::take(nearer).or(look),
                None => into.kinds.push((*kind, look.clone())),
            }
        }
    }
    folded
}

fn looks_info(looks: &WidgetLooks, palette: &ResolvedPalette) -> WidgetLooksInfo {
    WidgetLooksInfo {
        all: look_info(&looks.all, palette),
        kinds: looks
            .kinds
            .iter()
            .map(|(kind, look)| WidgetKindLookInfo {
                kind: kind.name().to_string(),
                look: look_info(look, palette),
            })
            .collect(),
    }
}

fn look_info(look: &WidgetLook, palette: &ResolvedPalette) -> WidgetLookInfo {
    WidgetLookInfo {
        view: look.view.map(|view| snake(&format!("{view:?}"))),
        color: look.color.as_ref().map(written),
        color_hex: look
            .color
            .as_ref()
            .and_then(|color| resolved_hex(palette, color)),
        background: look.background.as_ref().map(written),
        background_hex: look
            .background
            .as_ref()
            .and_then(|color| resolved_hex(palette, color)),
        opacity: look.opacity,
        graph: look.graph.map(|graph| graph.name().to_string()),
        bar: look.bar.map(|bar| bar.name().to_string()),
        segments: look.segments,
        sweep: look.sweep,
        thickness: look.thickness,
        radius: look.radius,
    }
}

fn data_url(canvas: &Canvas) -> Option<String> {
    let jpeg = canvas
        .to_jpeg(85)
        .map_err(|e| log::warn!("encoding a theme preview: {e}"))
        .ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        crate::base64::encode(&jpeg)
    ))
}

fn is_valid_id(id: &str) -> bool {
    id.len() <= MAX_ID_LEN
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A colour as a file would have it.
fn written(color: &ColorRef) -> String {
    match color {
        ColorRef::Literal(rgb) => hex(*rgb),
        ColorRef::Token(name) => format!("@{name}"),
    }
}

/// A colour resolved, without complaint when it does not: the validator
/// has already said so.
fn resolved_hex(palette: &ResolvedPalette, color: &ColorRef) -> Option<String> {
    palette.resolve(color, "", &mut Diagnostics::new()).map(hex)
}

fn written_style(value: StyleValue<'_>) -> Option<String> {
    match value {
        StyleValue::Color(color) => color.map(written),
        StyleValue::Size(size) => size.map(|size| size.to_string()),
        StyleValue::Pixels(pixels) => pixels.map(|pixels| pixels.to_string()),
    }
}

fn kind_of(value: ResolvedValue) -> &'static str {
    match value {
        ResolvedValue::Color(_) => "color",
        ResolvedValue::Size(_) => "size",
        ResolvedValue::Pixels(_) => "pixels",
    }
}

fn resolved_text(value: ResolvedValue) -> String {
    match value {
        ResolvedValue::Color(rgb) => hex(rgb),
        ResolvedValue::Size(size) => size.to_string(),
        ResolvedValue::Pixels(pixels) => pixels.to_string(),
    }
}

/// A unit variant's name as a config file spells it: `FollowMe` is
/// `follow_me`.
fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_a_plain_file_name() {
        assert!(is_valid_id("nord"));
        assert!(is_valid_id("nord-bright_2"));
        assert!(is_valid_id("8bit"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("-nord"));
        assert!(!is_valid_id("Nord"));
        assert!(!is_valid_id("../nord"));
        assert!(!is_valid_id("nord.toml"));
        assert!(!is_valid_id(&"a".repeat(MAX_ID_LEN + 1)));
    }

    #[test]
    fn variant_names_come_out_as_config_spells_them() {
        assert_eq!(snake("Wave"), "wave");
        assert_eq!(snake("FollowBackground"), "follow_background");
    }
}
