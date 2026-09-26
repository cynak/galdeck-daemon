//! Made-up readings, for drawing a widget that is not running.
//!
//! An editor's gallery shows what each widget looks like before it is placed.
//! Live readings would make the gallery show whatever this machine happens to
//! be doing -- an idle CPU draws as a flat line, and nothing playing draws as
//! an empty card -- which is a poor advertisement for either. These are the
//! same every time, so the preview is of the widget and not of the moment.

use std::sync::Arc;
use std::time::Duration;

use galdeck_core::Tick;
use galdeck_model::{Units, Widget, WidgetKind};

use super::media::{Art, Media, Status};
use super::weather::{Condition, Day, Weather};
use super::{Reading, SlotState};
use crate::countdown::Countdown;

/// A reading, with a history behind it, for any widget.
pub fn state(widget: &Widget) -> SlotState {
    let mut state = SlotState::default();
    let reading = match widget.kind {
        WidgetKind::Clock | WidgetKind::Date => {
            let now = super::now_in(widget.timezone());
            let time = now.time();
            Reading::Value {
                text: now.strftime(widget.format()).to_string(),
                // A clock face needs the time as a number.
                value: Some(
                    f64::from(time.hour()) * 3600.0
                        + f64::from(time.minute()) * 60.0
                        + f64::from(time.second()),
                ),
            }
        }
        WidgetKind::Uptime => Reading::Value {
            text: "3d 4h".into(),
            value: None,
        },
        WidgetKind::Weather => Reading::Weather(weather(widget.units())),
        WidgetKind::Media => Reading::Media(Some(media())),
        // Ready to start: its whole length, or a Pomodoro's when the length
        // is not one yet, and a stopwatch at nothing.
        WidgetKind::Timer => {
            let length = widget.duration().unwrap_or(Duration::from_secs(25 * 60));
            Reading::Value {
                text: Countdown::new(Some(length)).display(Tick::ZERO).text,
                value: Some(length.as_secs_f64()),
            }
        }
        WidgetKind::Stopwatch => Reading::Value {
            text: Countdown::new(None).display(Tick::ZERO).text,
            value: None,
        },
        kind => {
            // A wandering line with some shape to it, ending on the value
            // the text reports.
            let (level, swing, text) = match kind {
                WidgetKind::Cpu => (34.0, 18.0, "39%".to_string()),
                WidgetKind::Memory => (58.0, 4.0, "61%".to_string()),
                WidgetKind::Gpu => (47.0, 22.0, "54%".to_string()),
                WidgetKind::Disk => (41.0, 0.5, "41%".to_string()),
                WidgetKind::Temperature => match widget.units() {
                    Units::Celsius => (52.0, 9.0, "54°C".to_string()),
                    Units::Fahrenheit => (126.0, 16.0, "129°F".to_string()),
                },
                WidgetKind::Network => (2.2e6, 1.8e6, "↓1.2M ↑86K".to_string()),
                WidgetKind::Battery => (80.0, 1.5, "78% ↑".to_string()),
                WidgetKind::Fan => (1800.0, 350.0, "1850 rpm".to_string()),
                WidgetKind::Load => (1.1, 0.5, "1.34".to_string()),
                WidgetKind::Volume => (45.0, 0.0, "45%".to_string()),
                _ => (42.0, 12.0, "42".to_string()),
            };
            for i in 0..widget.history() {
                let x = i as f64;
                let value = level
                    + swing
                        * ((x / 5.0).sin() * 0.6 + (x / 2.3).sin() * 0.3 + (x / 11.0).cos() * 0.4);
                state.update(
                    Some(Reading::Value {
                        text: text.clone(),
                        value: Some(value.max(0.0)),
                    }),
                    widget,
                );
            }
            return state;
        }
    };
    state.update(Some(reading), widget);
    state
}

fn weather(units: Units) -> Weather {
    let f = |c: f64| match units {
        Units::Celsius => c,
        Units::Fahrenheit => c * 9.0 / 5.0 + 32.0,
    };
    let day = |label: &str, high: f64, low: f64, condition| Day {
        label: label.into(),
        high: f(high),
        low: f(low),
        condition,
    };
    Weather {
        temperature: f(21.0),
        condition: Condition::PartlyCloudy,
        is_day: true,
        units,
        days: vec![
            day("Thu", 24.0, 13.0, Condition::PartlyCloudy),
            day("Fri", 19.0, 11.0, Condition::Rain),
            day("Sat", 23.0, 12.0, Condition::Clear),
            day("Sun", 17.0, 9.0, Condition::Storm),
        ],
    }
}

fn media() -> Media {
    // A cover drawn rather than shipped: a diagonal dusk gradient.
    let art = image::RgbaImage::from_fn(96, 96, |x, y| {
        let t = (x + y) as f32 / 190.0;
        let mix = |a: f32, b: f32| (a + (b - a) * t) as u8;
        image::Rgba([mix(94.0, 236.0), mix(129.0, 112.0), mix(245.0, 168.0), 255])
    });
    Media {
        player: "spotify".into(),
        status: Status::Playing,
        title: Some("Dreamy Stupor".into()),
        artist: Some("Beats by Harris".into()),
        album: None,
        position: Some(Duration::from_secs(31)),
        length: Some(Duration::from_secs(120)),
        art: Some(Art {
            url: "demo".into(),
            image: Arc::new(art),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck_model::WidgetView;

    #[test]
    fn every_kind_has_something_to_show() {
        for kind in [
            WidgetKind::Clock,
            WidgetKind::Date,
            WidgetKind::Cpu,
            WidgetKind::Memory,
            WidgetKind::Temperature,
            WidgetKind::Gpu,
            WidgetKind::Network,
            WidgetKind::Disk,
            WidgetKind::Weather,
            WidgetKind::Media,
            WidgetKind::Command,
            WidgetKind::Battery,
            WidgetKind::Fan,
            WidgetKind::Load,
            WidgetKind::Uptime,
            WidgetKind::Volume,
            WidgetKind::Timer,
            WidgetKind::Stopwatch,
        ] {
            let widget = Widget {
                view: Some(WidgetView::Graph),
                ..Widget::of(kind)
            };
            let state = state(&widget);
            assert!(state.reading.is_some(), "{kind:?}");
            if kind.is_numeric() {
                assert_eq!(state.history.len(), widget.history(), "{kind:?}");
            }
        }
    }

    #[test]
    fn a_timer_shows_its_whole_length_and_a_stopwatch_nothing_yet() {
        let label = |widget: Widget| state(&widget).reading.and_then(|r| r.label());
        assert_eq!(
            label(Widget::of(WidgetKind::Timer)).as_deref(),
            Some("25:00")
        );
        let tea = Widget {
            duration: Some("4m".into()),
            ..Widget::of(WidgetKind::Timer)
        };
        assert_eq!(label(tea).as_deref(), Some("4:00"));
        assert_eq!(
            label(Widget::of(WidgetKind::Stopwatch)).as_deref(),
            Some("0:00")
        );
    }
}
