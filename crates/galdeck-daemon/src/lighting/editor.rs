//! What an editor needs of the keyboard's lighting: its layout to draw it
//! from, frames as text, and lighting drawn before it is saved -- by the same
//! renderer that lights the keyboard, so a preview cannot disagree with it.

use std::fmt::Write;

use galdeck::keyboard::{layout, Led, LightFrame, LED_COUNT};
use galdeck_ipc::{LedPlace, LightPress, LightingForm, LightingPreview, ReactiveForm};
use galdeck_model::lighting::{unknown_key, ReactiveEffect};
use galdeck_model::{ColorRef, Diagnostic, Diagnostics, Lighting, LightingEffect, ResolvedPalette};

use super::effects::{Press, Renderer};

/// The longest and densest a preview may be. Past this it is a recording,
/// and a reply the size of one.
const MAX_SECONDS: f32 = 10.0;
const MAX_FPS: u8 = 30;
const DEFAULT_SECONDS: f32 = 3.0;
const DEFAULT_FPS: u8 = 20;

/// Every LED that lights something, for drawing the keyboard.
pub fn places() -> Vec<LedPlace> {
    layout()
        .iter()
        .map(|info| LedPlace {
            index: info.led.index(),
            name: info.name.to_string(),
            x: info.x,
            y: info.y,
            group: info
                .led
                .group()
                .map_or("", |group| group.name())
                .to_string(),
        })
        .collect()
}

/// A frame as text: `rrggbb` for each LED, in frame order, run together.
pub fn frame_text(frame: &LightFrame) -> String {
    let mut out = String::with_capacity(LED_COUNT * 6);
    for c in frame.colors() {
        let _ = write!(out, "{:02x}{:02x}{:02x}", c.r, c.g, c.b);
    }
    out
}

/// Draw `text`, a `[lighting]` table, with its colours resolved against
/// `palette` and `presses` answered along the way. Drawn even when it has
/// problems, with whatever still resolves; the problems come with it.
pub fn preview(
    text: &str,
    palette: &ResolvedPalette,
    seconds: Option<f32>,
    fps: Option<u8>,
    presses: &[LightPress],
) -> LightingPreview {
    let mut out = Diagnostics::new();
    let (lighting, form) = match toml::from_str::<Lighting>(text) {
        Ok(lighting) => {
            let form = form(&lighting);
            (lighting, Some(form))
        }
        Err(error) => {
            out.push(Diagnostic::error(
                "E0001",
                "lighting",
                error.message().to_string(),
            ));
            (Lighting::default(), None)
        }
    };
    lighting.check("lighting", &mut out);
    let resolved = lighting.resolve(palette, "lighting", &mut out);
    let off = resolved.effect == LightingEffect::Off;
    let (renderer, unknown) = Renderer::new(resolved);
    for name in unknown {
        out.push(unknown_key("lighting.keys", &name));
    }
    let presses: Vec<Press> = presses
        .iter()
        .filter_map(|press| {
            let found = Led::key(&press.key).and_then(|led| Press::new(led, f64::from(press.at)));
            if found.is_none() {
                out.push(unknown_key("presses", &press.key));
            }
            found
        })
        .collect();

    let fps = fps.unwrap_or(DEFAULT_FPS).clamp(1, MAX_FPS);
    let seconds = seconds
        .filter(|s| s.is_finite())
        .unwrap_or(DEFAULT_SECONDS)
        .clamp(0.0, MAX_SECONDS);
    let count = ((seconds * f32::from(fps)).round() as usize).max(1);
    let mut frame = LightFrame::new();
    let frames = (0..count)
        .map(|i| {
            if off {
                // Handed back, the keyboard shows its own effects, which
                // nothing here knows: shown dark.
                frame.fill(galdeck::Rgb::BLACK);
            } else {
                renderer.render(i as f64 / f64::from(fps), &presses, &mut frame);
            }
            frame_text(&frame)
        })
        .collect();
    LightingPreview {
        fps,
        frames,
        diagnostics: out.iter().cloned().collect(),
        form,
    }
}

/// `lighting` as it was written, for an editor to change and write back.
pub fn form(lighting: &Lighting) -> LightingForm {
    LightingForm {
        effect: lighting
            .effect
            .map(|effect| effect_name(effect).to_string()),
        colors: lighting
            .colors
            .as_ref()
            .map(|colors| colors.iter().map(written).collect()),
        speed: lighting.speed,
        brightness: lighting.brightness,
        bar: lighting.bar.as_ref().map(written),
        keys: lighting
            .keys
            .iter()
            .map(|(names, color)| (names.clone(), written(color)))
            .collect(),
        reactive: lighting.reactive.as_ref().map(|reactive| ReactiveForm {
            effect: reactive.effect.map(|effect| {
                match effect {
                    ReactiveEffect::Ripple => "ripple",
                    ReactiveEffect::Glow => "glow",
                    ReactiveEffect::None => "none",
                }
                .to_string()
            }),
            color: reactive.color.as_ref().map(written),
            fade_ms: reactive.fade_ms,
        }),
    }
}

/// A colour as it is written in a config: `@name`, or `#rrggbb`.
fn written(color: &ColorRef) -> String {
    match color {
        ColorRef::Token(name) => format!("@{name}"),
        ColorRef::Literal(rgb) => format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b),
    }
}

fn effect_name(effect: LightingEffect) -> &'static str {
    match effect {
        LightingEffect::Static => "static",
        LightingEffect::Gradient => "gradient",
        LightingEffect::Breathe => "breathe",
        LightingEffect::Wave => "wave",
        LightingEffect::Spectrum => "spectrum",
        LightingEffect::Off => "off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck::Rgb;

    fn a(text: &str) -> Rgb {
        let at = usize::from(Led::key("A").unwrap().index()) * 6;
        let hex = &text[at..at + 6];
        Rgb::from_hex(hex).unwrap()
    }

    #[test]
    fn every_lit_led_has_a_place_and_a_group() {
        let places = places();
        assert_eq!(places.len(), layout().len());
        assert!(places.iter().all(|place| !place.group.is_empty()));
        let w = places.iter().find(|place| place.name == "W").unwrap();
        assert_eq!((w.index, w.group.as_str()), (26, "letters"));
    }

    #[test]
    fn a_frame_is_six_hex_digits_per_led() {
        let mut frame = LightFrame::new();
        frame.set(Led::key("A").unwrap(), Rgb::new(0x12, 0xab, 0xff));
        let text = frame_text(&frame);
        assert_eq!(text.len(), LED_COUNT * 6);
        assert_eq!(a(&text), Rgb::new(0x12, 0xab, 0xff));
    }

    #[test]
    fn a_preview_is_drawn_as_the_keyboard_would_be() {
        let preview = preview(
            "effect = \"static\"\ncolors = [\"#ff0000\"]\nbrightness = 100",
            &ResolvedPalette::default(),
            Some(1.0),
            Some(10),
            &[],
        );
        assert_eq!(preview.frames.len(), 10);
        assert!(preview.diagnostics.is_empty(), "{:#?}", preview.diagnostics);
        assert_eq!(a(&preview.frames[0]), Rgb::RED);
    }

    #[test]
    fn a_press_is_answered_in_the_preview() {
        let lighting = "colors = [\"#000000\"]\nbrightness = 100\n[reactive]\neffect = \"glow\"\ncolor = \"#ffffff\"";
        let press = LightPress {
            key: "A".into(),
            at: 0.5,
        };
        let preview = preview(
            lighting,
            &ResolvedPalette::default(),
            Some(1.0),
            Some(10),
            &[press],
        );
        assert_eq!(a(&preview.frames[4]), Rgb::BLACK, "before the press");
        assert_eq!(a(&preview.frames[5]), Rgb::WHITE, "as it is pressed");
    }

    #[test]
    fn problems_come_with_the_drawing() {
        let broken = preview("effect = ", &ResolvedPalette::default(), None, None, &[]);
        assert!(broken.diagnostics.iter().any(|d| d.code == "E0001"));
        assert_eq!(broken.frames.len(), 60, "three seconds at twenty frames");
        let unknown = preview(
            "[keys]\n\"nope letters\" = \"#ffffff\"",
            &ResolvedPalette::default(),
            None,
            None,
            &[],
        );
        assert!(unknown.diagnostics.iter().any(|d| d.code == "W0205"));
    }

    #[test]
    fn the_lighting_comes_back_as_it_was_written() {
        let text = "effect = \"wave\"\ncolors = [\"@accent\", \"#FF0000\"]\nspeed = 0.5\n\
                    [keys]\nletters = \"#00ff00\"\n[reactive]\neffect = \"glow\"\nfade_ms = 400";
        let form = preview(text, &ResolvedPalette::default(), None, None, &[])
            .form
            .unwrap();
        assert_eq!(form.effect.as_deref(), Some("wave"));
        assert_eq!(form.colors.unwrap(), ["@accent", "#ff0000"]);
        assert_eq!(form.speed, Some(0.5));
        assert_eq!(form.brightness, None, "left out stays left out");
        assert_eq!(form.keys["letters"], "#00ff00");
        let reactive = form.reactive.unwrap();
        assert_eq!(
            (reactive.effect.as_deref(), reactive.fade_ms),
            (Some("glow"), Some(400))
        );
        assert_eq!(reactive.color, None);
        // Text that does not read has no form to change.
        assert!(
            preview("effect = ", &ResolvedPalette::default(), None, None, &[])
                .form
                .is_none()
        );
    }

    #[test]
    fn a_preview_is_bounded() {
        let long = preview("", &ResolvedPalette::default(), Some(600.0), Some(240), &[]);
        assert_eq!(long.fps, MAX_FPS);
        assert_eq!(
            long.frames.len(),
            (MAX_SECONDS * f32::from(MAX_FPS)) as usize
        );
    }
}
