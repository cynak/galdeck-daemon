//! Lighting effects: the colour of every LED at a moment.
//!
//! A pure function of the configuration, each LED's position and a time in
//! seconds. Nothing here counts frames, so an effect moves at the same pace
//! whatever the frame rate, and any instant of it can be tested directly.

use galdeck::keyboard::{layout, select, Led, LedInfo, LightFrame, BAR_SEGMENTS};
use galdeck::Rgb;
use galdeck_device::LED_GAMMA;
use galdeck_model::lighting::ReactiveEffect;
use galdeck_model::{LightingEffect, ResolvedLighting};

/// The dimmest a breath gets, as a share of full light. At zero the keyboard
/// reads as switching off between breaths.
const BREATHE_FLOOR: f32 = 0.08;
/// How fast a ripple spreads, in key widths a second: across the keyboard
/// in about a second.
const RIPPLE_SPEED: f64 = 18.0;
/// How wide a ripple's ring is, in key widths.
const RIPPLE_WIDTH: f64 = 1.1;

/// A key going down, as the lighting answers it: where the key is, and when.
/// Nothing else about a press is kept.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    led: Led,
    x: f32,
    y: f32,
    /// Seconds in, on the effects' clock.
    at: f64,
}

impl Press {
    /// A press of the key under `led`, `at` seconds in. `None` for an LED
    /// with no place on the keyboard, which nothing could ripple out from.
    pub fn new(led: Led, at: f64) -> Option<Press> {
        let info = led.info()?;
        Some(Press {
            led,
            x: info.x,
            y: info.y,
            at,
        })
    }

    /// Whether it has faded completely `t` seconds in, answered as `reaction`.
    pub fn is_over(&self, t: f64, reaction: &Reaction) -> bool {
        t - self.at >= reaction.fade
    }
}

/// How the keys answer presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionKind {
    /// The key flares and fades.
    Glow,
    /// The key flashes, and a ring spreads from it across the keyboard.
    Ripple,
}

/// Keys answering presses, over the effect.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reaction {
    pub kind: ReactionKind,
    pub color: Rgb,
    /// Seconds a press takes to fade out.
    pub fade: f64,
}

/// Lighting made ready to draw: key names looked up once, and the keyboard's
/// extent measured once.
#[derive(Clone, Debug, PartialEq)]
pub struct Renderer {
    lighting: ResolvedLighting,
    keys: Vec<(Led, Rgb)>,
    reaction: Option<Reaction>,
    left: f32,
    width: f32,
}

impl Renderer {
    /// Prepare `lighting`, with the names in it that name no key or group.
    /// A name may be a key's (`W`), a group's (`letters`) or `all`.
    pub fn new(lighting: ResolvedLighting) -> (Self, Vec<String>) {
        let mut keys = Vec::new();
        let mut unknown = Vec::new();
        for (names, color) in &lighting.keys {
            for name in names.split_whitespace() {
                match select(name) {
                    Some(leds) => keys.extend(leds.into_iter().map(|led| (led, *color))),
                    None => unknown.push(name.to_string()),
                }
            }
        }
        let left = layout()
            .iter()
            .map(|info| info.x)
            .fold(f32::INFINITY, f32::min);
        let right = layout()
            .iter()
            .map(|info| info.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let reaction = lighting.reactive.map(|reactive| Reaction {
            kind: match reactive.effect {
                ReactiveEffect::Glow => ReactionKind::Glow,
                // `none` never gets this far: it resolves to no reaction.
                ReactiveEffect::Ripple | ReactiveEffect::None => ReactionKind::Ripple,
            },
            color: reactive.color,
            fade: f64::from(reactive.fade_ms) / 1000.0,
        });
        let renderer = Renderer {
            lighting,
            keys,
            reaction,
            left,
            width: (right - left).max(1.0),
        };
        (renderer, unknown)
    }

    /// The same, answering presses as `reaction` says.
    pub fn with_reaction(self, reaction: Option<Reaction>) -> Self {
        Renderer { reaction, ..self }
    }

    /// How it answers presses, if it does.
    pub fn reaction(&self) -> Option<&Reaction> {
        self.reaction.as_ref()
    }

    /// Whether the picture changes with time. One that does not is drawn once.
    pub fn moves(&self) -> bool {
        matches!(
            self.lighting.effect,
            LightingEffect::Breathe | LightingEffect::Wave | LightingEffect::Spectrum
        )
    }

    /// Draw the moment `t` seconds in, with `presses` answered over it, as a
    /// screen would show it: the keyboard turns it into light on the way out.
    pub fn render(&self, t: f64, presses: &[Press], frame: &mut LightFrame) {
        let brightness = level_for_light(f32::from(self.lighting.brightness) / 100.0);
        // LEDs that light nothing are sent dark; every frame carries them.
        frame.fill(Rgb::BLACK);
        for info in layout() {
            frame.set(info.led, self.effect_at(info.x, t).scaled(brightness));
        }
        if let Some(bar) = self.lighting.bar {
            for led in (0..BAR_SEGMENTS).filter_map(Led::bar) {
                frame.set(led, bar.scaled(brightness));
            }
        }
        for (led, color) in &self.keys {
            frame.set(*led, color.scaled(brightness));
        }
        if let Some(reaction) = &self.reaction {
            let color = reaction.color.scaled(brightness);
            for info in layout() {
                let strength = reaction.strength(info, presses, t);
                if strength > 0.0 {
                    let under = frame.get(info.led);
                    frame.set(info.led, under.lerp(color, strength as f32));
                }
            }
        }
    }

    /// The effect's colour at `x` key widths from the left, `t` seconds in.
    fn effect_at(&self, x: f32, t: f64) -> Rgb {
        let colors = &self.lighting.colors;
        let across = f64::from((x - self.left) / self.width);
        let cycles = t * self.lighting.speed;
        match self.lighting.effect {
            LightingEffect::Static | LightingEffect::Off => colors[0],
            LightingEffect::Gradient => spread(colors, across),
            LightingEffect::Wave => around(colors, (across - cycles).rem_euclid(1.0)),
            LightingEffect::Breathe => {
                // Darkest where a cycle starts, so each breath can bring in the
                // next colour without a visible jump.
                let breath = cycles.floor().rem_euclid(colors.len() as f64) as usize;
                let rising = (0.5 - 0.5 * (std::f64::consts::TAU * cycles).cos()) as f32;
                // Levels are already spaced as the eye sees them, a screen's
                // and so the keyboard's, so an even ramp of level is an even
                // breath.
                let floor = level_for_light(BREATHE_FLOOR);
                colors[breath].scaled(floor + (1.0 - floor) * rising)
            }
            LightingEffect::Spectrum => Rgb::from_hsv(cycles.rem_euclid(1.0) as f32, 1.0, 1.0),
        }
    }
}

impl Reaction {
    /// How much of its colour an LED takes from `presses`, `t` seconds in:
    /// the strongest of them, so presses in a row overlap rather than add up.
    fn strength(&self, info: &LedInfo, presses: &[Press], t: f64) -> f64 {
        let mut strongest: f64 = 0.0;
        for press in presses {
            let age = t - press.at;
            if !(0.0..self.fade).contains(&age) {
                continue;
            }
            let left = 1.0 - age / self.fade;
            let here = match self.kind {
                // Quick to fall and slow to finish, as light from a flash does.
                ReactionKind::Glow => f64::from(u8::from(info.led == press.led)) * left * left,
                // Evenly, so the ring is still there to see as it reaches the
                // far side of the keyboard.
                ReactionKind::Ripple => {
                    let dx = f64::from(info.x - press.x);
                    let dy = f64::from(info.y - press.y);
                    let distance = (dx * dx + dy * dy).sqrt();
                    let ring = (-((distance - age * RIPPLE_SPEED) / RIPPLE_WIDTH).powi(2)).exp();
                    // The key itself flashes as the ring leaves it.
                    ring.max(f64::from(u8::from(info.led == press.led))) * left
                }
            };
            strongest = strongest.max(here);
        }
        strongest
    }
}

/// The factor to scale a colour's levels by for it to give `share` of its
/// light: brightness is a share of the light, as the keyboard gives it.
fn level_for_light(share: f32) -> f32 {
    share.max(0.0).powf(1.0 / LED_GAMMA)
}

/// The colours spread evenly from `at` 0 to 1, first to last.
fn spread(colors: &[Rgb], at: f64) -> Rgb {
    let last = colors.len() - 1;
    if last == 0 {
        return colors[0];
    }
    let position = at.clamp(0.0, 1.0) * last as f64;
    let from = (position.floor() as usize).min(last - 1);
    colors[from].lerp(colors[from + 1], (position - from as f64) as f32)
}

/// The colours around a loop: `at` 0 and 1 are both the first, so a pattern
/// scrolling through it never shows a seam.
fn around(colors: &[Rgb], at: f64) -> Rgb {
    let count = colors.len();
    let position = at.rem_euclid(1.0) * count as f64;
    let from = (position.floor() as usize).min(count - 1);
    colors[from].lerp(colors[(from + 1) % count], (position - from as f64) as f32)
}

/// Each LED `amount` of the way from `from` to `to`.
pub fn blend(from: &LightFrame, to: &LightFrame, amount: f32) -> LightFrame {
    let mut out = LightFrame::new();
    for led in Led::all() {
        out.set(led, from.get(led).lerp(to.get(led), amount));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck_device::led_level;

    const RED: Rgb = Rgb::RED;
    const BLUE: Rgb = Rgb::BLUE;

    fn lighting(effect: LightingEffect, colors: &[Rgb]) -> ResolvedLighting {
        ResolvedLighting {
            effect,
            colors: colors.to_vec(),
            speed: 0.5,
            brightness: 100,
            bar: None,
            keys: Vec::new(),
            reactive: None,
        }
    }

    fn draw(lighting: ResolvedLighting, t: f64) -> LightFrame {
        let mut frame = LightFrame::new();
        Renderer::new(lighting).0.render(t, &[], &mut frame);
        frame
    }

    fn key(name: &str) -> Led {
        Led::key(name).unwrap()
    }

    #[test]
    fn static_lights_every_key_and_nothing_else() {
        let frame = draw(lighting(LightingEffect::Static, &[RED]), 0.0);
        for info in layout() {
            assert_eq!(frame.get(info.led), RED, "{}", info.name);
        }
        // Index 0 lights nothing, so it is sent dark.
        assert_eq!(frame.get(Led::new(0).unwrap()), Rgb::BLACK);
    }

    #[test]
    fn brightness_is_a_share_of_the_light() {
        let colour = Rgb::new(200, 100, 50);
        let mut half = lighting(LightingEffect::Static, &[colour]);
        half.brightness = 50;
        let drawn = draw(half.clone(), 0.0).get(key("A"));
        // On the keys, half the light in every channel, give or take the
        // rounding of a level: the colour dims without changing.
        for (dim, full) in [
            (drawn.r, colour.r),
            (drawn.g, colour.g),
            (drawn.b, colour.b),
        ] {
            let (dim, full) = (f32::from(led_level(dim)), f32::from(led_level(full)));
            assert!(
                (dim - full / 2.0).abs() <= 1.5,
                "{dim} is not half of {full}"
            );
        }
        half.brightness = 0;
        assert_eq!(draw(half, 0.0).get(key("A")), Rgb::BLACK);
    }

    #[test]
    fn a_gradient_runs_from_the_first_colour_to_the_last() {
        let frame = draw(lighting(LightingEffect::Gradient, &[RED, BLUE]), 0.0);
        assert_eq!(frame.get(key("Esc")), RED);
        assert_eq!(frame.get(key("Right")), BLUE);
        let middle = frame.get(key("G"));
        assert!(middle.r > 0 && middle.b > 0, "{middle:?} is not a mix");
    }

    #[test]
    fn a_wave_repeats_every_cycle_and_moves_between() {
        let wave = lighting(LightingEffect::Wave, &[RED, BLUE]);
        // At 0.5 cycles a second, a cycle takes two seconds. Times that are
        // exact in binary, so "the same" is not at the mercy of rounding.
        assert_eq!(draw(wave.clone(), 0.25), draw(wave.clone(), 2.25));
        assert_ne!(draw(wave.clone(), 0.25), draw(wave.clone(), 0.75));
        assert!(Renderer::new(wave).0.moves());
    }

    #[test]
    fn a_breath_is_darkest_at_the_start_and_brightest_halfway() {
        let breathe = lighting(LightingEffect::Breathe, &[RED, BLUE]);
        let dim = draw(breathe.clone(), 0.0).get(key("A"));
        let bright = draw(breathe.clone(), 1.0).get(key("A"));
        assert_eq!(bright, RED);
        // At its dimmest, a breath gives the keys about 8% of the light.
        let light = led_level(dim.r);
        assert!((18..=23).contains(&light), "{dim:?} gives {light}");
        // The next breath, two seconds later, is the next colour.
        assert_eq!(draw(breathe, 3.0).get(key("A")), BLUE);
    }

    #[test]
    fn spectrum_turns_the_whole_keyboard_through_the_hues() {
        let spectrum = lighting(LightingEffect::Spectrum, &[RED]);
        assert_eq!(draw(spectrum.clone(), 0.0).get(key("A")), RED);
        // A third of a cycle on is green.
        let green = draw(spectrum, 2.0 / 3.0).get(key("A"));
        assert!(green.g > 250 && green.r < 5 && green.b < 5, "{green:?}");
    }

    #[test]
    fn the_bar_and_named_keys_override_the_effect() {
        let mut lit = lighting(LightingEffect::Static, &[RED]);
        lit.bar = Some(BLUE);
        lit.keys = vec![("w a s d".to_string(), Rgb::GREEN)];
        let frame = draw(lit, 0.0);
        assert_eq!(frame.get(Led::bar(3).unwrap()), BLUE);
        for name in ["W", "A", "S", "D"] {
            assert_eq!(frame.get(key(name)), Rgb::GREEN, "{name}");
        }
        assert_eq!(frame.get(key("Q")), RED);
    }

    #[test]
    fn names_that_are_not_keys_are_reported() {
        let mut lit = lighting(LightingEffect::Static, &[RED]);
        lit.keys = vec![("esc nope".to_string(), BLUE)];
        let (renderer, unknown) = Renderer::new(lit);
        assert_eq!(unknown, ["nope"]);
        let mut frame = LightFrame::new();
        renderer.render(0.0, &[], &mut frame);
        assert_eq!(frame.get(key("Esc")), BLUE);
    }

    fn glow(fade: f64) -> Reaction {
        Reaction {
            kind: ReactionKind::Glow,
            color: Rgb::WHITE,
            fade,
        }
    }

    fn ripple(fade: f64) -> Reaction {
        Reaction {
            kind: ReactionKind::Ripple,
            ..glow(fade)
        }
    }

    /// Presses answered over a dark keyboard, `t` seconds in.
    fn react(reaction: Reaction, presses: &[Press], t: f64) -> LightFrame {
        let (renderer, _) = Renderer::new(lighting(LightingEffect::Static, &[Rgb::BLACK]));
        let mut frame = LightFrame::new();
        renderer
            .with_reaction(Some(reaction))
            .render(t, presses, &mut frame);
        frame
    }

    #[test]
    fn a_glow_lights_the_pressed_key_and_fades() {
        let a = key("A");
        let press = [Press::new(a, 1.0).unwrap()];
        assert_eq!(react(glow(0.5), &press, 1.0).get(a), Rgb::WHITE);
        let halfway = react(glow(0.5), &press, 1.25).get(a);
        assert!(halfway.r > 20 && halfway.r < 255, "{halfway:?}");
        assert_eq!(react(glow(0.5), &press, 1.5).get(a), Rgb::BLACK);
        // Only that key, and not before it was pressed.
        assert_eq!(react(glow(0.5), &press, 1.0).get(key("S")), Rgb::BLACK);
        assert_eq!(react(glow(0.5), &press, 0.9).get(a), Rgb::BLACK);
        assert!(press[0].is_over(1.5, &glow(0.5)) && !press[0].is_over(1.4, &glow(0.5)));
    }

    #[test]
    fn a_ripple_spreads_out_from_the_pressed_key() {
        // G and Enter are on the same row, 7.65 key widths apart.
        let (g, enter) = (key("G"), key("Enter"));
        let press = [Press::new(g, 0.0).unwrap()];
        let start = react(ripple(2.0), &press, 0.0);
        assert_eq!(start.get(g), Rgb::WHITE);
        assert!(start.get(enter).r < 5, "the ring has not reached Enter");
        let arriving = react(ripple(2.0), &press, 7.65 / RIPPLE_SPEED);
        assert!(arriving.get(enter).r > 100, "{:?}", arriving.get(enter));
        let after = react(ripple(2.0), &press, 2.0);
        assert!(layout()
            .iter()
            .all(|info| after.get(info.led) == Rgb::BLACK));
    }

    #[test]
    fn presses_overlap_rather_than_add_up() {
        let a = key("A");
        let one = [Press::new(a, 0.0).unwrap()];
        let twice = [one[0], one[0]];
        assert_eq!(react(glow(1.0), &one, 0.3), react(glow(1.0), &twice, 0.3));
    }

    #[test]
    fn a_reaction_is_as_bright_as_the_lighting() {
        let mut dim = lighting(LightingEffect::Static, &[Rgb::BLACK]);
        dim.brightness = 50;
        let a = key("A");
        let mut frame = LightFrame::new();
        Renderer::new(dim).0.with_reaction(Some(glow(1.0))).render(
            0.0,
            &[Press::new(a, 0.0).unwrap()],
            &mut frame,
        );
        let mut white = lighting(LightingEffect::Static, &[Rgb::WHITE]);
        white.brightness = 50;
        assert_eq!(frame.get(a), draw(white, 0.0).get(a));
    }

    #[test]
    fn the_config_says_how_presses_are_answered() {
        let mut lit = lighting(LightingEffect::Static, &[RED]);
        lit.reactive = Some(galdeck_model::lighting::ResolvedReactive {
            effect: ReactiveEffect::Glow,
            color: BLUE,
            fade_ms: 400,
        });
        let (renderer, _) = Renderer::new(lit);
        assert_eq!(
            renderer.reaction(),
            Some(&Reaction {
                kind: ReactionKind::Glow,
                color: BLUE,
                fade: 0.4,
            })
        );
        let (quiet, _) = Renderer::new(lighting(LightingEffect::Static, &[RED]));
        assert_eq!(quiet.reaction(), None);
    }

    #[test]
    fn a_press_needs_a_place_on_the_keyboard() {
        // Index 0 lights nothing, so nothing could spread from it.
        assert!(Press::new(Led::new(0).unwrap(), 0.0).is_none());
        assert!(Press::new(key("A"), 0.0).is_some());
    }

    #[test]
    fn a_blend_mixes_every_led() {
        let half = blend(&LightFrame::filled(RED), &LightFrame::filled(BLUE), 0.5);
        let mixed = half.get(key("A"));
        assert!(mixed.r > 100 && mixed.b > 100 && mixed.g == 0, "{mixed:?}");
        assert_eq!(
            blend(&LightFrame::filled(RED), &LightFrame::filled(BLUE), 1.0),
            LightFrame::filled(BLUE)
        );
    }
}
